use anyhow::{Context, Result};
use indicatif::ProgressBar;
use std::{
    backtrace::Backtrace,
    fs::{self, File},
    os::fd::IntoRawFd,
    path::Path,
    sync::atomic::Ordering,
    time::Instant,
};

use crate::managed_fd::ManagedFd;
use crate::pipeline::PipelineProcessor;
use crate::shm_manager::ShmManager;
use crate::task::CopyStats;

/// USRBIO文件拷贝器
///
/// 负责协调各个组件完成文件的USRBIO拷贝
pub struct USRBIOCopier {
    mount_point: String,
    block_size: usize,
    min_pipeline_depth: usize,
    max_pipeline_depth: usize,
    max_iov_size: usize, // 改为最大限制
    pub stats: CopyStats,
    pub progress_bar: Option<ProgressBar>,
    preserve_attrs: bool,
    debug: bool,
}

impl USRBIOCopier {
    pub fn new(
        mount_point: String,
        block_size: usize,
        min_pipeline_depth: usize,
        max_pipeline_depth: usize,
        max_iov_size: usize,
        stats: CopyStats,
        show_progress: bool,
        preserve_attrs: bool,
        debug: bool,
    ) -> Self {
        Self {
            mount_point,
            block_size,
            min_pipeline_depth,
            max_pipeline_depth,
            max_iov_size,
            stats,
            progress_bar: if show_progress {
                Some(ProgressBar::new(0))
            } else {
                None
            },
            preserve_attrs,
            debug,
        }
    }

    /// 计算实际需要的共享内存大小
    ///
    /// 策略：默认使用文件大小，但不超过max_iov_size，且至少能容纳一个块
    fn calculate_iov_size(&self, file_size: u64) -> usize {
        let size = file_size as usize;

        // 至少需要一个块的大小
        let min_size = self.block_size;

        // 使用文件大小，但限制在[min_size, max_iov_size]范围内
        size.max(min_size).min(self.max_iov_size)
    }

    /// 使用USRBIO拷贝单个文件
    pub fn copy_file(&self, src: &Path, dst: &Path) -> Result<()> {
        let start_time = Instant::now();

        self.log_debug(&format!("========== Starting copy =========="));
        self.log_debug(&format!("Source: {:?}", src));
        self.log_debug(&format!("Target: {:?}", dst));

        // 1. 打开源文件
        let mut src_file = File::open(src).with_context(|| {
            self.format_error(
                "Failed to open source file",
                &format!("Source path: {:?}", src),
            )
        })?;

        let metadata = src_file.metadata().with_context(|| {
            self.format_error("Failed to get metadata", &format!("Source file: {:?}", src))
        })?;

        let file_size = metadata.len();
        self.log_debug(&format!("File size: {} bytes", file_size));

        // 特殊处理：空文件
        if file_size == 0 {
            File::create(dst).with_context(|| format!("Failed to create: {:?}", dst))?;
            return Ok(());
        }

        // 2. 创建目标文件并注册FD
        self.log_debug(&format!("Creating target file: {:?}", dst));

        let dst_file = File::create(dst).with_context(|| {
            self.format_error(
                "Failed to create target file",
                &format!("Target path: {:?}", dst),
            )
        })?;

        let raw_fd = dst_file.into_raw_fd();
        self.log_debug(&format!("Registering fd: {}", raw_fd));

        let managed_fd = unsafe { ManagedFd::from_raw_fd(raw_fd) };
        self.log_debug("FD registered successfully");

        // 3. 计算实际需要的共享内存大小
        let iov_size = self.calculate_iov_size(file_size);
        self.log_debug(&format!(
            "Using iov_size: {} bytes (max: {} bytes)",
            iov_size, self.max_iov_size
        ));

        // 4. 创建共享内存和Iov
        let shm_manager = ShmManager::new(
            &self.mount_point,
            iov_size,
            self.debug,
            self.progress_bar.as_ref(),
        )?;

        // 5. 创建Pipeline处理器
        let pipeline = PipelineProcessor::new(
            self.mount_point.clone(),
            self.block_size,
            self.min_pipeline_depth,
            self.max_pipeline_depth,
            iov_size,
            self.stats.bytes_copied.clone(),
            self.progress_bar.clone(),
            self.debug,
        );

        // 5. 执行Pipeline拷贝
        pipeline.execute(
            &mut src_file,
            dst,
            managed_fd.as_raw_fd(),
            &shm_manager.iov,
            file_size,
            shm_manager.as_ptr(),
        )?;

        // 6. 同步文件长度
        self.log_debug("Syncing file with ioctl");
        let sync_start = Instant::now();

        unsafe {
            libc::ioctl(
                managed_fd.as_raw_fd(),
                hf3fs_usrbio_sys::HF3FS_SUPER_MAGIC as _,
            );
        }

        self.log_debug(&format!(
            "ioctl sync completed in {:.2}s",
            sync_start.elapsed().as_secs_f64()
        ));

        // 7. 保留文件属性
        if self.preserve_attrs {
            self.log_debug("Preserving file permissions");
            fs::set_permissions(dst, metadata.permissions()).with_context(|| {
                self.format_error("Failed to set permissions", &format!("Target file: {:?}", dst))
            })?;
        }

        self.stats.files_copied.fetch_add(1, Ordering::Relaxed);

        self.log_debug("========== Copy completed successfully ==========");
        self.log_debug(&format!("Time: {:.2}s", start_time.elapsed().as_secs_f64()));

        if let Some(pb) = &self.progress_bar {
            pb.println(format!(
                "✓ {:?} -> {:?} ({:.2}s)",
                src,
                dst,
                start_time.elapsed().as_secs_f64()
            ));
        }

        Ok(())
    }

    /// 输出debug日志
    fn log_debug(&self, msg: &str) {
        if self.debug {
            if let Some(pb) = &self.progress_bar {
                pb.println(format!("[DEBUG] {}", msg));
            }
        }
    }

    /// 格式化错误信息
    fn format_error(&self, prefix: &str, context: &str) -> String {
        if self.debug {
            let backtrace = Backtrace::capture();
            format!(
                "{}\n[DEBUG] {}\nBacktrace:\n{}",
                prefix, context, backtrace
            )
        } else {
            format!("{}: {}", prefix, context)
        }
    }
}
