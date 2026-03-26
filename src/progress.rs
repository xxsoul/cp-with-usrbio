use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// 进度状态文件
#[derive(Debug, Serialize, Deserialize)]
pub struct ProgressState {
    /// 源路径
    pub source: PathBuf,
    /// 目标路径
    pub target: PathBuf,
    /// 总文件数
    pub total_files: usize,
    /// 已完成的文件路径（相对路径）
    pub completed_files: HashSet<String>,
    /// 失败的文件路径（相对路径）
    pub failed_files: HashSet<String>,
    /// 创建时间戳
    pub created_at: u64,
    /// 最后更新时间戳
    pub updated_at: u64,
}

impl ProgressState {
    /// 创建新的进度状态
    pub fn new(source: PathBuf, target: PathBuf, total_files: usize) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            source,
            target,
            total_files,
            completed_files: HashSet::new(),
            failed_files: HashSet::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// 从文件加载进度
    pub fn load(progress_file: &Path) -> Result<Self> {
        let file = File::open(progress_file)
            .with_context(|| format!("Failed to open progress file: {:?}", progress_file))?;

        let reader = BufReader::new(file);
        let state: ProgressState = serde_json::from_reader(reader)
            .with_context(|| "Failed to parse progress file")?;

        Ok(state)
    }

    /// 保存进度到文件
    pub fn save(&self, progress_file: &Path) -> Result<()> {
        // 确保父目录存在
        if let Some(parent) = progress_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {:?}", parent))?;
        }

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(progress_file)
            .with_context(|| format!("Failed to create progress file: {:?}", progress_file))?;

        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, self)
            .with_context(|| "Failed to write progress file")?;

        Ok(())
    }

    /// 标记文件为已完成
    pub fn mark_completed(&mut self, relative_path: String) {
        self.completed_files.insert(relative_path);
        self.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }

    /// 标记文件为失败
    pub fn mark_failed(&mut self, relative_path: String) {
        self.failed_files.insert(relative_path);
        self.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }

    /// 检查文件是否已完成
    pub fn is_completed(&self, relative_path: &str) -> bool {
        self.completed_files.contains(relative_path)
    }

    /// 获取进度百分比
    pub fn progress_percent(&self) -> f64 {
        if self.total_files == 0 {
            return 0.0;
        }
        (self.completed_files.len() as f64 / self.total_files as f64) * 100.0
    }

    /// 获取进度文件路径（固定在目标目录下）
    pub fn get_progress_file_path(target: &Path) -> PathBuf {
        target.join(".cp-with-usrbio-progress.json")
    }
}

/// 进度管理器
pub struct ProgressManager {
    state: Option<ProgressState>,
    progress_file: PathBuf,
    enabled: bool,
    save_counter: usize,
    save_interval: usize,
}

impl ProgressManager {
    /// 创建进度管理器
    pub fn new(
        source: PathBuf,
        target: PathBuf,
        total_files: usize,
        enable_resume: bool,
    ) -> Result<Self> {
        let progress_file = ProgressState::get_progress_file_path(&target);
        let enabled = enable_resume && total_files > 100;

        let state = if enabled {
            // 尝试加载已有进度
            if progress_file.exists() {
                match ProgressState::load(&progress_file) {
                    Ok(existing_state) => {
                        // 验证路径是否匹配
                        if existing_state.source == source && existing_state.target == target {
                            println!(
                                "✓ Resuming from previous progress: {}/{} files ({:.1}%)",
                                existing_state.completed_files.len(),
                                existing_state.total_files,
                                existing_state.progress_percent()
                            );
                            Some(existing_state)
                        } else {
                            println!("⚠ Progress file exists but paths don't match, starting fresh");
                            Some(ProgressState::new(source, target, total_files))
                        }
                    }
                    Err(e) => {
                        println!("⚠ Failed to load progress file: {}, starting fresh", e);
                        Some(ProgressState::new(source, target, total_files))
                    }
                }
            } else {
                Some(ProgressState::new(source, target, total_files))
            }
        } else {
            None
        };

        Ok(Self {
            state,
            progress_file,
            enabled,
            save_counter: 0,
            save_interval: 10, // 每完成10个文件保存一次
        })
    }

    /// 检查文件是否需要处理
    pub fn should_process(&self, relative_path: &str) -> bool {
        if !self.enabled {
            return true;
        }

        if let Some(state) = &self.state {
            // 如果不在已完成列表中，需要处理
            if !state.is_completed(relative_path) {
                return true;
            }

            // 如果在已完成列表中，验证目标文件是否存在
            let target_path = state.target.join(relative_path);
            if !target_path.exists() {
                // 目标文件不存在，需要重新处理
                eprintln!("⚠ Target file missing, will re-copy: {}", relative_path);
                return true;
            }

            // 目标文件存在，跳过
            false
        } else {
            true
        }
    }

    /// 标记文件完成
    pub fn mark_completed(&mut self, relative_path: String) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        if let Some(state) = &mut self.state {
            state.mark_completed(relative_path);
            self.save_counter += 1;

            // 定期保存
            if self.save_counter >= self.save_interval {
                self.save_counter = 0;
                self.save()?;
            }
        }

        Ok(())
    }

    /// 标记文件失败
    pub fn mark_failed(&mut self, relative_path: String) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        if let Some(state) = &mut self.state {
            state.mark_failed(relative_path);
            self.save_counter += 1;

            if self.save_counter >= self.save_interval {
                self.save_counter = 0;
                self.save()?;
            }
        }

        Ok(())
    }

    /// 手动保存
    pub fn save(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        if let Some(state) = &self.state {
            state.save(&self.progress_file)?;
        }

        Ok(())
    }

    /// 获取统计信息
    pub fn get_stats(&self) -> Option<(usize, usize, usize)> {
        self.state.as_ref().map(|s| {
            (
                s.completed_files.len(),
                s.failed_files.len(),
                s.total_files,
            )
        })
    }

    /// 更新总文件数
    pub fn update_total_files(&mut self, total: usize) {
        if let Some(state) = &mut self.state {
            state.total_files = total;
        }
    }
}

impl Drop for ProgressManager {
    fn drop(&mut self) {
        // 退出时保存最终进度
        if self.enabled {
            if let Err(e) = self.save() {
                eprintln!("Warning: Failed to save progress on exit: {}", e);
            }
        }
    }
}
