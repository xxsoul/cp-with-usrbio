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
use task::{collect_tasks, CopyStats, CopyTask};
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

    // 验证共享内存容量限制
    if args.block_size > args.max_iov_size {
        return Err(anyhow::anyhow!(
            "block_size ({}) cannot be larger than max_iov_size ({})",
            args.block_size,
            args.max_iov_size
        ));
    }

    let max_depth_by_shm = args.max_iov_size / args.block_size;
    if args.pipeline_depth > max_depth_by_shm {
        return Err(anyhow::anyhow!(
            "pipeline_depth ({}) exceeds maximum depth ({}) allowed by max_iov_size/block_size ratio",
            args.pipeline_depth,
            max_depth_by_shm
        ));
    }

    if args.max_pipeline_depth > max_depth_by_shm {
        return Err(anyhow::anyhow!(
            "max_pipeline_depth ({}) exceeds maximum depth ({}) allowed by max_iov_size/block_size ratio",
            args.max_pipeline_depth,
            max_depth_by_shm
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
    println!("Block size: {} bytes", args.block_size);
    println!(
        "Pipeline depth: {} - {} (auto-adjusted per file)",
        args.pipeline_depth, args.max_pipeline_depth
    );
    println!("Max shared memory: {} bytes ({:.2} GB)", args.max_iov_size, args.max_iov_size as f64 / 1_000_000_000.0);
    println!("Resume enabled: {}", args.resume);
    if args.debug {
        println!("Debug mode: ENABLED");
    }
    println!();

    // 收集任务
    let tasks = collect_tasks(&source, &target, args.recursive)
        .context("Failed to collect copy tasks")?;

    let total_files = tasks.len();
    let total_bytes: u64 = tasks.iter().map(|t| t.file_size).sum();

    println!(
        "Found {} files, total size: {:.2} GB",
        total_files,
        total_bytes as f64 / 1_000_000_000.0
    );

    if total_files == 0 {
        println!("No files to copy");
        return Ok(());
    }

    // 创建进度管理器（文件数>100时启用断点续传）
    let progress_manager = Arc::new(std::sync::Mutex::new(
        ProgressManager::new(
            source.clone(),
            target.clone(),
            total_files,
            args.resume,
        )
        .context("Failed to create progress manager")?,
    ));

    // 如果有已完成的进度，调整统计
    if let Some((completed, failed, _)) = progress_manager.lock().unwrap().get_stats() {
        if completed > 0 {
            println!("Resuming: {} files already completed, {} failed", completed, failed);
        }
    }
    println!();

    // 创建统计和进度条
    let stats = CopyStats::new();
    let progress_bar = if args.progress {
        let pb = indicatif::ProgressBar::new(total_bytes);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})\n{msg}")
                .expect("Invalid progress template")
                .progress_chars("#>-")
        );
        Some(pb)
    } else {
        None
    };

    // 创建任务队列和工作线程
    let (sender, receiver) = channel::bounded::<CopyTask>(args.workers * 2);

    let start_time = Instant::now();

    // 启动工作线程
    let handles: Vec<_> = (0..args.workers)
        .map(|worker_id| {
            let receiver = receiver.clone();
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
                    args.max_iov_size,
                    stats_bytes,
                    progress_bar,
                    args.preserve_attrs,
                    args.debug,
                    progress_manager,
                );
            })
        })
        .collect();

    // 发送任务
    for task in tasks {
        sender.send(task).context("Failed to send task")?;
    }
    drop(sender); // 关闭通道

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
