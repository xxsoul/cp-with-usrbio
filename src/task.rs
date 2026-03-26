use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicU64,
};

use anyhow::{Context, Result};
use crossbeam::channel::Sender;
use walkdir::WalkDir;

/// 拷贝任务定义
#[derive(Debug)]
pub struct CopyTask {
    pub src_path: PathBuf,
    pub dst_path: PathBuf,
    pub file_size: u64,
    pub relative_path: String, // 相对路径，用于断点续传
}

/// 统计信息
#[derive(Debug, Clone)]
pub struct CopyStats {
    pub files_copied: std::sync::Arc<AtomicU64>,
    pub bytes_copied: std::sync::Arc<AtomicU64>,
    pub files_failed: std::sync::Arc<AtomicU64>,
}

impl CopyStats {
    pub fn new() -> Self {
        Self {
            files_copied: std::sync::Arc::new(AtomicU64::new(0)),
            bytes_copied: std::sync::Arc::new(AtomicU64::new(0)),
            files_failed: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }
}

/// 收集拷贝任务
pub fn collect_tasks(source: &Path, target: &Path, recursive: bool) -> Result<Vec<CopyTask>> {
    let mut tasks = Vec::new();

    if source.is_file() {
        let metadata = std::fs::metadata(source)?;
        let relative_path = source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();

        tasks.push(CopyTask {
            src_path: source.to_path_buf(),
            dst_path: target.to_path_buf(),
            file_size: metadata.len(),
            relative_path,
        });
    } else if source.is_dir() {
        if !recursive {
            return Err(anyhow::anyhow!(
                "Source is a directory, but --recursive flag is not set"
            ));
        }

        // 创建目标目录
        std::fs::create_dir_all(target)
            .with_context(|| format!("Failed to create target directory: {:?}", target))?;

        // 遍历源目录
        for entry in WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let src_path = entry.path();
                let rel_path = src_path.strip_prefix(source)?;
                let dst_path = target.join(rel_path);

                // 创建目标目录结构
                if let Some(parent) = dst_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create: {:?}", parent))?;
                }

                let metadata = entry.metadata()?;
                let relative_path = rel_path.to_str().unwrap_or("unknown").to_string();

                tasks.push(CopyTask {
                    src_path: src_path.to_path_buf(),
                    dst_path,
                    file_size: metadata.len(),
                    relative_path,
                });
            }
        }
    } else {
        return Err(anyhow::anyhow!("Source path does not exist: {:?}", source));
    }

    Ok(tasks)
}

/// Walk模式：边遍历边发送任务
///
/// 深度优先遍历，逐层创建目录结构并立即发送任务
/// 返回：(发现的文件数, 总字节数, sender)
pub fn walk_and_send_tasks(
    source: &Path,
    target: &Path,
    recursive: bool,
    sender: Sender<CopyTask>,
) -> Result<(usize, u64, Sender<CopyTask>)> {
    let mut total_files = 0;
    let mut total_bytes: u64 = 0;

    if source.is_file() {
        // 单文件模式
        let metadata = std::fs::metadata(source)?;
        let relative_path = source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();

        let task = CopyTask {
            src_path: source.to_path_buf(),
            dst_path: target.to_path_buf(),
            file_size: metadata.len(),
            relative_path,
        };

        sender.send(task).context("Failed to send single file task")?;

        return Ok((1, metadata.len(), sender));
    }

    if !source.is_dir() {
        return Err(anyhow::anyhow!("Source path does not exist: {:?}", source));
    }

    if !recursive {
        return Err(anyhow::anyhow!(
            "Source is a directory, but --recursive flag is not set"
        ));
    }

    // 创建目标根目录
    std::fs::create_dir_all(target)
        .with_context(|| format!("Failed to create target directory: {:?}", target))?;

    // 深度优先遍历
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let src_path = entry.path();

        if entry.file_type().is_dir() {
            // 遇到目录，立即创建对应的目标目录
            let rel_path = src_path.strip_prefix(source)?;
            let dst_dir = target.join(rel_path);

            std::fs::create_dir_all(&dst_dir)
                .with_context(|| format!("Failed to create directory: {:?}", dst_dir))?;

            // 打印目录创建信息（可选）
            // eprintln!("📁 Created: {:?}", dst_dir);
        } else if entry.file_type().is_file() {
            // 遇到文件，立即发送任务
            let rel_path = src_path.strip_prefix(source)?;
            let dst_path = target.join(rel_path);

            // 确保父目录存在（以防万一）
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create: {:?}", parent))?;
            }

            let metadata = entry.metadata()?;
            let relative_path = rel_path.to_str().unwrap_or("unknown").to_string();

            let task = CopyTask {
                src_path: src_path.to_path_buf(),
                dst_path,
                file_size: metadata.len(),
                relative_path,
            };

            // 发送任务到channel
            sender.send(task).context("Failed to send task")?;

            total_files += 1;
            total_bytes += metadata.len();

            // 定期打印进度（每100个文件）
            if total_files % 100 == 0 {
                eprintln!("🔍 Scanned {} files, {:.2} GB...", total_files, total_bytes as f64 / 1_000_000_000.0);
            }
        }
    }

    Ok((total_files, total_bytes, sender))
}
