mod cli;
mod managed_fd;
mod progress;
mod shm_manager;
mod task;
mod utils;
mod worker_context;

use anyhow::{Context, Result};
use clap::Parser;
use crossbeam::channel;
use indicatif::ProgressStyle;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};

use cli::Args;
use progress::ProgressManager;
use shm_manager::{cleanup_stale_shm, show_shm_status};
use task::{CopyStats, CopyTask};
use worker_context::worker_thread;

fn main() -> Result<()> {
    // 设置 Ctrl+C 处理器
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        eprintln!("\n⚠️  Received Ctrl+C signal, gracefully shutting down...");
        eprintln!("   Cleaning up resources (shared memory, file descriptors)...");
        r.store(false, Ordering::SeqCst);
    }).expect("Error setting Ctrl-C handler");

    let args = Args::parse();

    // 如果只是查看共享内存状态
    if args.shm_status {
        println!("🧹 Cleaning up stale shared memory files...");
        match cleanup_stale_shm() {
            Ok(cleaned) => println!("   Cleaned {} files\n", cleaned),
            Err(e) => eprintln!("   Warning: {}", e),
        }

        show_shm_status()?;
        return Ok(());
    }

    // 验证必需参数
    let source = args.source.ok_or_else(|| {
        anyhow::anyhow!("--source is required when not using --shm-status")
    })?;
    let target = args.target.ok_or_else(|| {
        anyhow::anyhow!("--target is required when not using --shm-status")
    })?;
    let mount_point = args.mount_point.ok_or_else(|| {
        anyhow::anyhow!("--mount-point is required when not using --shm-status")
    })?;

    // 如果开启了debug模式，设置环境变量以启用Rust的backtrace
    if args.debug {
        std::env::set_var("RUST_BACKTRACE", "full");
    }

    // 验证参数
    if !source.exists() {
        return Err(anyhow::anyhow!(
            "Source path does not exist: {:?}",
            source
        ));
    }

    // 验证pipeline_depth至少为1
    if args.pipeline_depth < 1 {
        return Err(anyhow::anyhow!(
            "pipeline_depth must be at least 1, got {}",
            args.pipeline_depth
        ));
    }

    if args.max_pipeline_depth < 1 {
        return Err(anyhow::anyhow!(
            "max_pipeline_depth must be at least 1, got {}",
            args.max_pipeline_depth
        ));
    }

    // 验证共享内存使用比例
    if args.shm_usage_ratio <= 0.0 || args.shm_usage_ratio > 1.0 {
        return Err(anyhow::anyhow!(
            "shm_usage_ratio must be between 0.0 and 1.0, got {}",
            args.shm_usage_ratio
        ));
    }

    // 清理遗留的共享内存文件
    match cleanup_stale_shm() {
        Ok(cleaned) if cleaned > 0 => {
            eprintln!("🧹 Cleaned {} stale shared memory files", cleaned);
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("Warning: Failed to cleanup stale shared memory: {}", e);
        }
    }

    // 计算每个 worker 的固定共享内存大小
    let total_shm = shm_manager::get_total_shm_size()?;
    let available_shm = shm_manager::get_shm_available_space()?;
    let usable_shm = (total_shm as f64 * args.shm_usage_ratio) as u64;

    // 确保不超过可用空间
    let actual_usable = usable_shm.min(available_shm);

    // 计算每个 worker 的固定大小
    let iov_size_per_worker = if args.workers > 0 {
        actual_usable / args.workers as u64
    } else {
        return Err(anyhow::anyhow!("workers must be at least 1"));
    };

    // 确保每个 worker 至少有一个 block_size
    let iov_size_per_worker = iov_size_per_worker.max(args.block_size as u64) as usize;

    // 验证 pipeline_depth 不超过 iov_size / block_size
    let max_depth_by_shm = iov_size_per_worker / args.block_size;
    if args.pipeline_depth > max_depth_by_shm {
        return Err(anyhow::anyhow!(
            "pipeline_depth ({}) exceeds maximum depth ({}) allowed by iov_size_per_worker/block_size ratio.\n\
             Try: reduce --workers or increase --shm-usage-ratio",
            args.pipeline_depth,
            max_depth_by_shm
        ));
    }

    if args.max_pipeline_depth > max_depth_by_shm {
        return Err(anyhow::anyhow!(
            "max_pipeline_depth ({}) exceeds maximum depth ({}) allowed by iov_size_per_worker/block_size ratio.\n\
             Try: reduce --workers or increase --shm-usage-ratio",
            args.max_pipeline_depth,
            max_depth_by_shm
        ));
    }

    // 显示共享内存状态
    if let Err(e) = show_shm_status() {
        eprintln!("Warning: Failed to get shared memory status: {}", e);
    }
    println!();

    println!("🚀 USRBIO Copy Tool");
    println!("Source: {:?}", source);
    println!("Target: {:?}", target);
    println!("Mount point: {}", mount_point);
    println!("Workers: {}", args.workers);
    println!("Block size: {} bytes ({:.2} MiB)", args.block_size, args.block_size as f64 / 1_048_576.0);
    println!(
        "Pipeline depth: {} - {} (auto-adjusted per file)",
        args.pipeline_depth, args.max_pipeline_depth
    );
    println!("Shared memory per worker: {} bytes ({:.2} MiB)", iov_size_per_worker, iov_size_per_worker as f64 / 1_048_576.0);
    println!("Total shared memory usage: {:.2} MiB ({:.0}% of total)",
        (iov_size_per_worker * args.workers) as f64 / 1_048_576.0,
        args.shm_usage_ratio * 100.0
    );
    println!("Resume enabled: {}", args.resume);
    if args.debug {
        println!("Debug mode: ENABLED");
    }
    println!();

    // 创建统计和进度条
    let stats = CopyStats::new();
    let progress_bar = if args.progress {
        let pb = indicatif::ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {msg}")
                .expect("Invalid progress template")
        );
        pb.enable_steady_tick(std::time::Duration::from_millis(200));
        pb.set_message("Scanning files...");
        Some(pb)
    } else {
        None
    };

    let start_time = Instant::now();

    // 创建进度管理器
    let progress_manager = Arc::new(std::sync::Mutex::new(
        ProgressManager::new(
            source.clone(),
            target.clone(),
            0, // 初始为0，后续更新
            args.resume,
        )
        .context("Failed to create progress manager")?,
    ));

    // 校验模式
    let mut initial_files_to_copy: Option<Vec<String>> = None;

    if args.verify {
        println!("🔍 Verifying file consistency...");
        println!();

        let verify_result = task::verify_files(&source, &target, args.recursive)
            .context("Failed to verify files")?;

        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("📊 Verification Result");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Files checked:    {}", verify_result.total_checked);
        println!("Missing files:    {}", verify_result.missing_files.len());
        println!("Size mismatch:    {}", verify_result.size_mismatch.len());
        println!("Total issues:     {}", verify_result.total_issues);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();

        if verify_result.total_issues == 0 {
            println!("✅ All files are consistent!");
            return Ok(());
        }

        // 显示问题文件
        if !verify_result.missing_files.is_empty() {
            println!("❌ Missing files ({} total):", verify_result.missing_files.len());
            let display_limit = 10;
            for (i, rel_path) in verify_result.missing_files.iter().enumerate() {
                if i >= display_limit {
                    println!("  ... and {} more", verify_result.missing_files.len() - display_limit);
                    break;
                }
                println!("  {}. {}", i + 1, rel_path);
            }
            println!();
        }

        if !verify_result.size_mismatch.is_empty() {
            println!("⚠️  Size mismatch files ({} total):", verify_result.size_mismatch.len());
            let display_limit = 10;
            for (i, rel_path) in verify_result.size_mismatch.iter().enumerate() {
                if i >= display_limit {
                    println!("  ... and {} more", verify_result.size_mismatch.len() - display_limit);
                    break;
                }
                println!("  {}. {}", i + 1, rel_path);
            }
            println!();
        }

        // 询问是否重传
        let should_retransmit = if args.yes {
            true
        } else {
            print!("🔄 Retransmit inconsistent files? [Y/n]: ");
            use std::io::{self, Write};
            io::stdout().flush().unwrap();

            let mut input = String::new();
            io::stdin().read_line(&mut input).unwrap();
            let input = input.trim().to_lowercase();

            input == "y" || input == "yes" || input.is_empty()
        };

        if should_retransmit {
            println!("📦 Preparing to retransmit {} files...",
                verify_result.total_issues);
            println!();

            // 合并问题文件列表
            let files_to_retransmit: Vec<String> = verify_result.missing_files
                .into_iter()
                .chain(verify_result.size_mismatch.into_iter())
                .collect();

            initial_files_to_copy = Some(files_to_retransmit);
        } else {
            println!("❌ Verification completed. No files retransmitted.");
            return Err(anyhow::anyhow!(
                "Found {} inconsistent files, user chose not to retransmit",
                verify_result.total_issues
            ));
        }
    }

    // 主循环：初次拷贝 + 重试失败任务
    let mut retry_count = 0;
    const MAX_RETRIES: usize = 3; // 最大重试次数限制
    let mut total_files: usize;
    let mut total_bytes: u64;
    let is_verify_retransmit = initial_files_to_copy.is_some();  // 标记是否是校验重传模式

    loop {
        if retry_count > 0 {
            println!("\n🔄 Retry attempt #{}", retry_count);
        }

        // 创建任务队列和工作线程
        let (sender, receiver) = channel::bounded::<CopyTask>(args.workers * 2);
        let receiver_for_workers = receiver.clone();

        // 启动工作线程
        let handles: Vec<_> = (0..args.workers)
            .map(|worker_id| {
                let receiver = receiver_for_workers.clone();
                let progress_manager = Arc::clone(&progress_manager);
                let mount_point = mount_point.clone();
                let stats_bytes = stats.bytes_copied.clone();
                let progress_bar = progress_bar.clone();
                let running = running.clone();

                thread::spawn(move || {
                    worker_thread(
                        worker_id,
                        receiver,
                        mount_point,
                        args.block_size,
                        args.pipeline_depth,
                        args.max_pipeline_depth,
                        iov_size_per_worker,
                        stats_bytes,
                        progress_bar,
                        args.preserve_attrs,
                        args.debug,
                        progress_manager,
                        running,
                    );
                })
            })
            .collect();

        // drop原始receiver，避免死锁
        drop(receiver);

        // 发送任务
        if retry_count == 0 {
            if is_verify_retransmit {
                // 校验重传模式：使用预先确定的问题文件列表
                let files_to_retransmit = initial_files_to_copy.take().unwrap();
                total_files = files_to_retransmit.len();

                println!("📤 Sending {} retransmit tasks...", total_files);

                // 更新进度管理器的总文件数
                progress_manager.lock().unwrap().update_total_files(total_files);

                // 由于是重传，不关心断点续传，直接发送所有任务
                // 计算总字节数（需要遍历文件获取大小）
                total_bytes = 0;
                for rel_path in &files_to_retransmit {
                    let src_path = source.join(rel_path);
                    if src_path.exists() {
                        if let Ok(metadata) = std::fs::metadata(&src_path) {
                            total_bytes += metadata.len();
                        }
                    }
                }

                // 更新进度条
                if let Some(ref pb) = progress_bar {
                    pb.set_length(total_bytes);
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})\n{msg}")
                            .expect("Invalid progress template")
                            .progress_chars("#>-")
                    );
                    pb.set_message("");
                }

                // 发送任务
                for rel_path in files_to_retransmit {
                    let src_path = source.join(&rel_path);
                    let dst_path = target.join(&rel_path);

                    if src_path.exists() {
                        let task = CopyTask {
                            src_path,
                            dst_path,
                            file_size: 0, // 将在worker中获取
                            relative_path: rel_path,
                        };
                        if let Err(e) = sender.send(task) {
                            eprintln!("Failed to send retransmit task: {}", e);
                        }
                    } else {
                        eprintln!("⚠️  Source file no longer exists: {:?}", src_path);
                    }
                }

                drop(sender);

                println!("Total size to retransmit: {:.2} GiB", total_bytes as f64 / 1_073_741_824.0);
                println!();

                if total_files == 0 {
                    println!("No files to retransmit");
                    return Ok(());
                }
            } else {
                // 正常模式：Walk模式，边遍历边发送任务
                println!("🚶 Walking directory and sending tasks...");
                let (files, bytes, sender) = task::walk_and_send_tasks(
                    &source,
                    &target,
                    args.recursive,
                    sender,
                ).context("Failed to walk and send tasks")?;
                total_files = files;
                total_bytes = bytes;

                // drop sender关闭通道
                drop(sender);

                // 更新进度管理器的总文件数
                progress_manager.lock().unwrap().update_total_files(total_files);

                // 如果有已完成的进度，显示
                if let Some((completed, failed, _)) = progress_manager.lock().unwrap().get_stats() {
                    if completed > 0 {
                        println!("Resuming: {} files already completed, {} failed", completed, failed);
                    }
                }

                // 更新进度条为实际的总字节数
                if let Some(ref pb) = progress_bar {
                    pb.set_length(total_bytes);
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})\n{msg}")
                            .expect("Invalid progress template")
                            .progress_chars("#>-")
                    );
                    pb.set_message("");
                }

                println!(
                    "Found {} files, total size: {:.2} GiB",
                    total_files,
                    total_bytes as f64 / 1_073_741_824.0
                );
                println!();

                if total_files == 0 {
                    println!("No files to copy");
                    return Ok(());
                }
            }
        } else {
            // 重试：只发送失败的任务
            let failed_files = progress_manager.lock().unwrap().get_failed_files();

            if failed_files.is_empty() {
                println!("No failed tasks to retry");
                break;
            }

            println!("Retrying {} failed tasks...", failed_files.len());

            // 清空失败列表，准备重试
            progress_manager.lock().unwrap().clear_failed_files();

            // 重置统计
            stats.files_failed.store(0, Ordering::Relaxed);

            // 发送失败的任务
            for rel_path in &failed_files {
                let src_path = source.join(rel_path);
                let dst_path = target.join(rel_path);

                if src_path.exists() {
                    let task = CopyTask {
                        src_path,
                        dst_path,
                        file_size: 0, // 将在worker中重新获取
                        relative_path: rel_path.clone(),
                    };
                    if let Err(e) = sender.send(task) {
                        eprintln!("Failed to send retry task: {}", e);
                    }
                } else {
                    eprintln!("Source file no longer exists: {:?}", src_path);
                }
            }

            drop(sender);
        }

        // 等待所有工作线程完成
        for handle in handles {
            let _ = handle.join();  // 忽略错误，继续清理
        }

        // 检查是否被中断
        if !running.load(Ordering::SeqCst) {
            // 先保存进度
            progress_manager.lock().unwrap().save()?;

            println!();
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("⚠️  Copy interrupted by user");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("Resources cleaned up:");
            println!("  ✓ Shared memory released");
            println!("  ✓ File descriptors closed");
            println!("  ✓ Progress saved");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!();
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("📋 Parameters Used");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("Source:             {:?}", source);
            println!("Target:             {:?}", target);
            println!("Mount point:        {}", mount_point);
            println!("Workers:            {}", args.workers);
            println!("Block size:         {} bytes ({:.2} MiB)", args.block_size, args.block_size as f64 / 1_048_576.0);
            println!("Pipeline depth:     {} - {}", args.pipeline_depth, args.max_pipeline_depth);
            println!("SHM usage ratio:    {:.2}%", args.shm_usage_ratio * 100.0);
            println!("Recursive:          {}", args.recursive);
            println!("Resume:             {}", args.resume);
            println!("Preserve attrs:     {}", args.preserve_attrs);
            println!("Progress:           {}", args.progress);
            if args.debug {
                println!("Debug:              {}", args.debug);
            }
            if args.verify {
                println!("Verify:             {}", args.verify);
                println!("Auto-confirm:       {}", args.yes);
            }
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!();

            // 返回中断错误
            return Err(anyhow::anyhow!("Copy interrupted by user (Ctrl+C)"));
        }

        // 保存进度
        progress_manager.lock().unwrap().save()?;

        // 如果是初次拷贝，打印统计信息
        if retry_count == 0 {
            let elapsed = start_time.elapsed();
            let files_copied = stats.files_copied.load(Ordering::Relaxed);
            let bytes_copied = stats.bytes_copied.load(Ordering::Relaxed);
            let files_failed = stats.files_failed.load(Ordering::Relaxed);

            println!();
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("✨ Copy completed!");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("Files copied:   {}", files_copied);
            println!("Files failed:   {}", files_failed);
            println!(
                "Bytes copied:   {:.2} GiB",
                bytes_copied as f64 / 1_073_741_824.0
            );
            println!("Time elapsed:   {:.2}s", elapsed.as_secs_f64());
            if files_copied > 0 {
                println!(
                    "Throughput:     {:.2} MiB/s",
                    (bytes_copied as f64 / 1_048_576.0) / elapsed.as_secs_f64()
                );
            }
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!();
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("📋 Parameters Used");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("Source:             {:?}", source);
            println!("Target:             {:?}", target);
            println!("Mount point:        {}", mount_point);
            println!("Workers:            {}", args.workers);
            println!("Block size:         {} bytes ({:.2} MiB)", args.block_size, args.block_size as f64 / 1_048_576.0);
            println!("Pipeline depth:     {} - {}", args.pipeline_depth, args.max_pipeline_depth);
            println!("SHM usage ratio:    {:.2}%", args.shm_usage_ratio * 100.0);
            println!("Recursive:          {}", args.recursive);
            println!("Resume:             {}", args.resume);
            println!("Preserve attrs:     {}", args.preserve_attrs);
            println!("Progress:           {}", args.progress);
            if args.debug {
                println!("Debug:              {}", args.debug);
            }
            if args.verify {
                println!("Verify:             {}", args.verify);
                println!("Auto-confirm:       {}", args.yes);
            }
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        }

        // 检查是否有失败任务
        let failed_files = progress_manager.lock().unwrap().get_failed_files();

        if !failed_files.is_empty() {
            println!();
            println!("❌ Failed files ({} total):", failed_files.len());
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

            // 限制显示数量，避免刷屏
            let display_limit = 20;
            for (i, rel_path) in failed_files.iter().enumerate() {
                if i >= display_limit {
                    println!("  ... and {} more", failed_files.len() - display_limit);
                    break;
                }
                println!("  {}. {}", i + 1, rel_path);
            }
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!();

            // 询问用户是否重试
            if retry_count < MAX_RETRIES - 1 {
                print!("🔄 Retry failed tasks? [Y/n]: ");
                use std::io::{self, Write};
                io::stdout().flush().unwrap();

                let mut input = String::new();
                io::stdin().read_line(&mut input).unwrap();
                let input = input.trim().to_lowercase();

                if input == "y" || input == "yes" || input.is_empty() {
                    retry_count += 1;
                    // 继续循环进行重试
                    continue;
                } else {
                    break;
                }
            } else {
                println!("⚠️ Maximum retry attempts ({}) reached.", MAX_RETRIES);
                break;
            }
        } else {
            // 没有失败任务，退出循环
            break;
        }
    }

    // 最终结果
    let final_failed = progress_manager.lock().unwrap().get_failed_files();
    if !final_failed.is_empty() {
        Err(anyhow::anyhow!(
            "{} files failed to copy after {} retry attempts",
            final_failed.len(),
            retry_count
        ))
    } else {
        Ok(())
    }
}
