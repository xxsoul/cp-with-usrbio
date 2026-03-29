use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicU64,
};

use anyhow::{Context, Result};
use crossbeam::channel::Sender;
use walkdir::WalkDir;

/// 文件校验结果
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub missing_files: Vec<String>,      // target缺失的文件
    pub size_mismatch: Vec<String>,      // 大小不一致的文件
    pub total_checked: usize,            // 总检查文件数
    pub total_bytes: u64,                // 总文件容量（字节）
    pub total_issues: usize,             // 总问题文件数
}

/// 拷贝任务定义
#[derive(Debug)]
pub struct CopyTask {
    pub src_path: PathBuf,
    pub dst_path: PathBuf,
    #[allow(dead_code)]
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
#[allow(dead_code)]
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
                eprintln!("🔍 Scanned {} files, {:.2} GiB...", total_files, total_bytes as f64 / 1_073_741_824.0);
            }
        }
    }

    Ok((total_files, total_bytes, sender))
}

/// 校验source和target文件一致性（仅比较文件大小）
///
/// 返回：需要重传的文件列表（相对路径）
pub fn verify_files(
    source: &Path,
    target: &Path,
    recursive: bool,
) -> Result<VerifyResult> {
    let mut missing_files = Vec::new();
    let mut size_mismatch = Vec::new();
    let mut total_checked = 0;
    let mut total_bytes: u64 = 0;

    if source.is_file() {
        // 单文件模式
        total_checked = 1;

        let src_metadata = std::fs::metadata(source)
            .with_context(|| format!("Failed to get metadata: {:?}", source))?;

        total_bytes = src_metadata.len();

        if !target.exists() {
            let relative_path = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string();
            missing_files.push(relative_path);
        } else {
            let dst_metadata = std::fs::metadata(target)
                .with_context(|| format!("Failed to get metadata: {:?}", target))?;

            if src_metadata.len() != dst_metadata.len() {
                let relative_path = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                size_mismatch.push(relative_path);
            }
        }
    } else if source.is_dir() {
        if !recursive {
            return Err(anyhow::anyhow!(
                "Source is a directory, but --recursive flag is not set"
            ));
        }

        // 遍历源目录
        for entry in WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                total_checked += 1;

                let src_path = entry.path();
                let rel_path = src_path.strip_prefix(source)?;
                let dst_path = target.join(rel_path);

                // 获取源文件大小
                let src_metadata = match entry.metadata() {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("Warning: Failed to get metadata for {:?}: {}", src_path, e);
                        continue;
                    }
                };

                total_bytes += src_metadata.len();

                // 检查目标文件是否存在
                if !dst_path.exists() {
                    let relative_path = rel_path.to_str().unwrap_or("unknown").to_string();
                    missing_files.push(relative_path);
                    continue;
                }

                // 检查文件大小
                let dst_metadata = match std::fs::metadata(&dst_path) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("Warning: Failed to get metadata for {:?}: {}", dst_path, e);
                        continue;
                    }
                };

                if src_metadata.len() != dst_metadata.len() {
                    let relative_path = rel_path.to_str().unwrap_or("unknown").to_string();
                    size_mismatch.push(relative_path);
                }
            }
        }
    } else {
        return Err(anyhow::anyhow!("Source path does not exist: {:?}", source));
    }

    let total_issues = missing_files.len() + size_mismatch.len();

    Ok(VerifyResult {
        missing_files,
        size_mismatch,
        total_checked,
        total_bytes,
        total_issues,
    })
}
