use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicU64,
};

use anyhow::{Context, Result};
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
