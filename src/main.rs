mod cli;
mod managed_fd;
mod pipeline;
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
        atomic::Ordering,
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
    println!("Block size: {} bytes ({:.2} MB)", args.block_size, args.block_size as f64 / 1_000_000.0);
    println!(
        "Pipeline depth: {} - {} (auto-adjusted per file)",
        args.pipeline_depth, args.max_pipeline_depth
    );
    println!("Shared memory per worker: {} bytes ({:.2} MB)", iov_size_per_worker, iov_size_per_worker as f64 / 1_000_000.0);
    println!("Total shared memory usage: {:.2} MB ({:.0}% of total)",
        (iov_size_per_worker * args.workers) as f64 / 1_000_000.0,
        args.shm_usage_ratio * 100.0
    );
    println!("Resume enabled: {}", args.resume);
    if args.debug {
        println!("Debug mode: ENABLED");
    }
    println!();

    // 创建统计和进度条（先用spinner，后面更新总数）
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

    // 创建任务队列和工作线程
    let (sender, receiver) = channel::bounded::<CopyTask>(args.workers * 2);

    let start_time = Instant::now();

    // 先启动工作线程
    // 注意：此时还不知道总文件数，后续更新
    let progress_manager = Arc::new(std::sync::Mutex::new(
        ProgressManager::new(
            source.clone(),
            target.clone(),
            0, // 初始为0，后续更新
            args.resume,
        )
        .context("Failed to create progress manager")?,
    ));

    // 克隆receiver用于workers，保留原始sender用于发送任务
    let receiver_for_workers = receiver.clone();

    // 启动工作线程
    let handles: Vec<_> = (0..args.workers)
        .map(|worker_id| {
            let receiver = receiver_for_workers.clone();
            let progress_manager = Arc::clone(&progress_manager);
            let mount_point = mount_point.clone();
            let stats_bytes = stats.bytes_copied.clone();
            let progress_bar = progress_bar.clone();

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
                );
            })
        })
        .collect();

    // drop原始receiver，避免死锁
    drop(receiver);

    // Walk模式：边遍历边发送任务
    println!("🚶 Walking directory and sending tasks...");
    let (total_files, total_bytes, sender) = task::walk_and_send_tasks(
        &source,
        &target,
        args.recursive,
        sender,
    ).context("Failed to walk and send tasks")?;

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
        "Found {} files, total size: {:.2} GB",
        total_files,
        total_bytes as f64 / 1_000_000_000.0
    );
    println!();

    if total_files == 0 {
        println!("No files to copy");
        return Ok(());
    }

    // sender已在walk_and_send_tasks中drop，无需手动drop

    // 等待所有工作线程完成
    for handle in handles {
        handle.join().expect("Worker thread panicked");
    }

    // 最终保存进度
    progress_manager.lock().unwrap().save()?;

    // 打印最终统计
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
        "Bytes copied:   {:.2} GB",
        bytes_copied as f64 / 1_000_000_000.0
    );
    println!("Time elapsed:   {:.2}s", elapsed.as_secs_f64());
    if files_copied > 0 {
        println!(
            "Throughput:     {:.2} MB/s",
            (bytes_copied as f64 / 1_000_000.0) / elapsed.as_secs_f64()
        );
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    if files_failed > 0 {
        Err(anyhow::anyhow!("{} files failed to copy", files_failed))
    } else {
        Ok(())
    }
}
