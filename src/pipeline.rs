use anyhow::{Context, Result};
use hf3fs_usrbio_sys::{Ior, Iov};
use indicatif::ProgressBar;
use std::{
    backtrace::Backtrace,
    collections::VecDeque,
    fs::File,
    io::Read,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

/// Pipeline I/O 处理器
///
/// 负责管理USRBIO的pipeline I/O操作
pub struct PipelineProcessor {
    mount_point: String,
    block_size: usize,
    min_pipeline_depth: usize,
    max_pipeline_depth: usize,
    iov_size: usize,
    stats_bytes: Arc<AtomicU64>,
    progress_bar: Option<ProgressBar>,
    debug: bool,
}

impl PipelineProcessor {
    pub fn new(
        mount_point: String,
        block_size: usize,
        min_pipeline_depth: usize,
        max_pipeline_depth: usize,
        iov_size: usize,
        stats_bytes: Arc<AtomicU64>,
        progress_bar: Option<ProgressBar>,
        debug: bool,
    ) -> Self {
        Self {
            mount_point,
            block_size,
            min_pipeline_depth,
            max_pipeline_depth,
            iov_size,
            stats_bytes,
            progress_bar,
            debug,
        }
    }

    /// 计算最优的pipeline深度
    pub fn calculate_pipeline_depth(&self, file_size: u64) -> usize {
        // 基于共享内存容量的最大深度
        let max_depth_by_shm = self.iov_size / self.block_size;
        if max_depth_by_shm == 0 {
            return 1;
        }

        // 根据文件大小计算理想深度
        let ideal_depth = if file_size < 10 * 1024 * 1024 {
            // < 10MB
            self.min_pipeline_depth.max(1)
        } else if file_size < 100 * 1024 * 1024 {
            // 10MB - 100MB
            let depth = (file_size / (20 * 1024 * 1024)) as usize + 1;
            depth.clamp(self.min_pipeline_depth, 4)
        } else if file_size < 1024 * 1024 * 1024 {
            // 100MB - 1GB
            let depth = (file_size / (200 * 1024 * 1024)) as usize * 2 + 2;
            depth.clamp(4, 8)
        } else {
            // > 1GB
            self.max_pipeline_depth.min(8)
        };

        ideal_depth
            .min(max_depth_by_shm)
            .min(self.max_pipeline_depth)
    }

    /// 执行pipeline拷贝
    pub fn execute(
        &self,
        src_file: &mut File,
        dst: &Path,
        fd: i32,
        iov: &Iov,
        file_size: u64,
        shm_ptr: *mut u8,
    ) -> Result<()> {
        let pipeline_depth = self.calculate_pipeline_depth(file_size);

        if pipeline_depth == 0 {
            return Err(anyhow::anyhow!(
                "Calculated pipeline_depth is 0, invalid parameters"
            ));
        }

        if self.debug {
            if let Some(pb) = &self.progress_bar {
                pb.println(format!(
                    "[DEBUG] Creating Ior: pipeline_depth={}",
                    pipeline_depth
                ));
                pb.println(format!(
                    "[DEBUG] Pipeline depth auto-adjusted: {} (file_size: {} bytes)",
                    pipeline_depth, file_size
                ));
            }
        }

        // 创建Ior（批处理模式）
        let ior = Ior::create(
            &self.mount_point,
            false, // for_write
            pipeline_depth as i32,
            -(pipeline_depth as i32), // 批处理模式
            1000,                     // timeout
            -1,                       // numa
            0,                        // flags
        )
        .map_err(|e| {
            let backtrace = Backtrace::capture();
            anyhow::anyhow!(
                "Failed to create Ior: {}\n\
                 [DEBUG] Pipeline depth: {}\n\
                 [DEBUG] Target: {:?}\n\
                 Backtrace:\n{}",
                e, pipeline_depth, dst, backtrace
            )
        })?;

        // 执行pipeline
        self.run_pipeline(src_file, fd, iov, &ior, file_size, shm_ptr, pipeline_depth)
    }

    /// 运行pipeline循环
    fn run_pipeline(
        &self,
        src_file: &mut File,
        fd: i32,
        iov: &Iov,
        ior: &Ior,
        file_size: u64,
        shm_ptr: *mut u8,
        pipeline_depth: usize,
    ) -> Result<()> {
        let mut inflight: VecDeque<(usize, usize, usize)> = VecDeque::new();
        let mut src_offset: usize = 0;
        let mut completed_bytes: usize = 0;
        let mut buf_offset_counter: usize = 0;
        let mut last_progress_time = Instant::now();
        let mut last_completed_bytes = 0;
        let mut consecutive_timeout_count = 0;
        const MAX_CONSECUTIVE_TIMEOUTS: usize = 5;
        let mut need_submit = false;

        while completed_bytes < file_size as usize {
            // 检测剩余I/O数量
            let remaining_bytes = file_size as usize - src_offset;
            let remaining_ios = (remaining_bytes + self.block_size - 1) / self.block_size;

            // 提前处理最后不足一批的I/O
            if remaining_ios < pipeline_depth && remaining_ios > 0 && inflight.is_empty() {
                self.handle_final_partial_batch(
                    src_file,
                    fd,
                    iov,
                    shm_ptr,
                    file_size as usize,
                    src_offset,
                    &mut completed_bytes,
                )?;
                break;
            }

            // 阶段1: 准备I/O
            while inflight.len() < pipeline_depth && src_offset < file_size as usize {
                let buf_offset = (buf_offset_counter * self.block_size) % self.iov_size;
                let chunk_size = std::cmp::min(self.block_size, file_size as usize - src_offset);

                // 检查缓冲区边界
                if buf_offset + chunk_size > self.iov_size {
                    if self.debug {
                        if let Some(pb) = &self.progress_bar {
                            pb.println(format!(
                                "[DEBUG] Buffer wrap-around at offset {}, waiting",
                                src_offset
                            ));
                        }
                    }
                    break;
                }

                // 读取数据
                unsafe {
                    let buf_slice =
                        std::slice::from_raw_parts_mut(shm_ptr.add(buf_offset), chunk_size);
                    src_file.read_exact(buf_slice).with_context(|| {
                        if self.debug {
                            let backtrace = Backtrace::capture();
                            format!(
                                "Failed to read from source\n\
                                 [DEBUG] Buffer offset: {}\n\
                                 [DEBUG] Chunk size: {}\n\
                                 [DEBUG] Source offset: {}\n\
                                 Backtrace:\n{}",
                                buf_offset, chunk_size, src_offset, backtrace
                            )
                        } else {
                            "Failed to read from source".to_string()
                        }
                    })?;
                }

                // 准备I/O请求
                ior.prepare_raw(
                    iov,
                    buf_offset..buf_offset + chunk_size,
                    fd,
                    src_offset,
                    (src_offset, chunk_size),
                )
                .map_err(|e| {
                    let backtrace = Backtrace::capture();
                    anyhow::anyhow!(
                        "Failed to prepare I/O: {}\n\
                         [DEBUG] Buffer offset: {}\n\
                         [DEBUG] Chunk size: {}\n\
                         [DEBUG] File offset: {}\n\
                         Backtrace:\n{}",
                        e, buf_offset, chunk_size, src_offset, backtrace
                    )
                })?;

                inflight.push_back((src_offset, chunk_size, buf_offset));
                src_offset += chunk_size;
                buf_offset_counter += 1;
                need_submit = true;
            }

            // 阶段2: 提交并等待完成
            if !inflight.is_empty() {
                let is_final_batch = src_offset >= file_size as usize;
                let is_partial_batch = inflight.len() < pipeline_depth;

                // 处理最后一批且数量不足的情况
                if is_final_batch && is_partial_batch && inflight.len() > 0 {
                    self.handle_final_batch_with_new_ior(
                        src_file,
                        fd,
                        iov,
                        &mut inflight,
                        &mut completed_bytes,
                    )?;
                    break;
                }

                // 提交I/O
                if need_submit {
                    if self.debug {
                        if let Some(pb) = &self.progress_bar {
                            pb.println(format!(
                                "[DEBUG] Submitting {} I/O requests{}",
                                inflight.len(),
                                if is_final_batch { " (final batch)" } else { "" }
                            ));
                        }
                    }

                    ior.submit();
                    need_submit = false;

                    if is_final_batch {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        ior.submit();
                    }
                }

                // 等待完成
                let max_poll = std::cmp::min(inflight.len(), pipeline_depth);
                let completed = ior.poll::<(usize, usize)>(1..=max_poll, 30000);

                if completed.is_empty() && !inflight.is_empty() {
                    consecutive_timeout_count += 1;

                    if let Some(pb) = &self.progress_bar {
                        pb.println(format!(
                            "[WARNING] Poll timeout #{}! {} I/Os in-flight",
                            consecutive_timeout_count,
                            inflight.len()
                        ));
                    }

                    if consecutive_timeout_count >= MAX_CONSECUTIVE_TIMEOUTS {
                        return Err(anyhow::anyhow!(
                            "Poll timeout {} times. {} I/Os in-flight. Completed: {} / {}",
                            consecutive_timeout_count,
                            inflight.len(),
                            completed_bytes,
                            file_size
                        ));
                    }

                    if is_final_batch {
                        ior.submit();
                    }
                    continue;
                } else {
                    consecutive_timeout_count = 0;
                }

                // 进度卡住检测
                let now = Instant::now();
                if completed_bytes > last_completed_bytes {
                    last_completed_bytes = completed_bytes;
                    last_progress_time = now;
                } else if now.duration_since(last_progress_time).as_secs() > 60 {
                    return Err(anyhow::anyhow!(
                        "Copy stuck: no progress for 60s. Completed {} / {}",
                        completed_bytes,
                        file_size
                    ));
                }

                // 处理完成的I/O
                for io in completed {
                    if io.result < 0 {
                        return Err(anyhow::anyhow!(
                            "Write failed at offset {}: error {}",
                            io.extra.0,
                            io.result
                        ));
                    }
                    if io.result as usize != io.extra.1 {
                        return Err(anyhow::anyhow!(
                            "Write incomplete: expected {}, got {}",
                            io.extra.1,
                            io.result
                        ));
                    }

                    completed_bytes += io.result as usize;
                    inflight.pop_front();

                    self.stats_bytes
                        .fetch_add(io.result as u64, Ordering::Relaxed);
                    if let Some(pb) = &self.progress_bar {
                        pb.inc(io.result as u64);
                    }
                }
            }
        }

        // 清理剩余inflight
        if !inflight.is_empty() {
            self.drain_remaining_ios(ior, &mut inflight, &mut completed_bytes)?;
        }

        // 最终验证
        if completed_bytes != file_size as usize {
            return Err(anyhow::anyhow!(
                "Size mismatch: completed {} != expected {}",
                completed_bytes,
                file_size
            ));
        }

        Ok(())
    }

    /// 处理最后不足一批的I/O（提前检测）
    fn handle_final_partial_batch(
        &self,
        src_file: &mut File,
        fd: i32,
        iov: &Iov,
        shm_ptr: *mut u8,
        file_size: usize,
        src_offset: usize,
        completed_bytes: &mut usize,
    ) -> Result<()> {
        let remaining_bytes = file_size - src_offset;
        let remaining_ios = (remaining_bytes + self.block_size - 1) / self.block_size;

        if self.debug {
            if let Some(pb) = &self.progress_bar {
                pb.println(format!(
                    "[DEBUG] Final partial batch: {} I/Os remaining",
                    remaining_ios
                ));
            }
        }

        // 创建专用Ior
        let final_ior = Ior::create(
            &self.mount_point,
            false,
            remaining_ios as i32,
            0,    // io_depth = 0
            1000,
            -1,
            0,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create final Ior: {}", e))?;

        let mut final_src_offset = src_offset;
        let mut final_buf_offset_counter = 0;
        let mut final_inflight: VecDeque<(usize, usize, usize)> = VecDeque::new();

        // 准备I/O
        while final_src_offset < file_size {
            let buf_offset = (final_buf_offset_counter * self.block_size) % self.iov_size;
            let chunk_size = std::cmp::min(self.block_size, file_size - final_src_offset);

            if buf_offset + chunk_size > self.iov_size {
                break;
            }

            unsafe {
                let buf_slice = std::slice::from_raw_parts_mut(shm_ptr.add(buf_offset), chunk_size);
                src_file.read_exact(buf_slice)?;
            }

            final_ior
                .prepare_raw(
                    iov,
                    buf_offset..buf_offset + chunk_size,
                    fd,
                    final_src_offset,
                    (final_src_offset, chunk_size),
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to prepare final I/O: offset={}, size={}, error={}",
                        final_src_offset,
                        chunk_size,
                        e
                    )
                })?;

            final_inflight.push_back((final_src_offset, chunk_size, buf_offset));
            final_src_offset += chunk_size;
            final_buf_offset_counter += 1;
        }

        // 提交并等待
        final_ior.submit();
        let completed = final_ior.poll::<(usize, usize)>(1..=final_inflight.len(), 60000);

        if completed.is_empty() {
            return Err(anyhow::anyhow!(
                "Final batch poll timeout with {} I/Os",
                final_inflight.len()
            ));
        }

        for io in completed {
            if io.result < 0 {
                return Err(anyhow::anyhow!(
                    "Final write failed at offset {}: error {}",
                    io.extra.0,
                    io.result
                ));
            }
            if io.result as usize != io.extra.1 {
                return Err(anyhow::anyhow!(
                    "Final write incomplete: expected {}, got {}",
                    io.extra.1,
                    io.result
                ));
            }

            *completed_bytes += io.result as usize;
            self.stats_bytes
                .fetch_add(io.result as u64, Ordering::Relaxed);
            if let Some(pb) = &self.progress_bar {
                pb.inc(io.result as u64);
            }
        }

        Ok(())
    }

    /// 处理最后一批（创建新Ior）
    fn handle_final_batch_with_new_ior(
        &self,
        _src_file: &mut File,
        fd: i32,
        iov: &Iov,
        inflight: &mut VecDeque<(usize, usize, usize)>,
        completed_bytes: &mut usize,
    ) -> Result<()> {
        // 同步之前的写入
        unsafe {
            libc::ioctl(fd, hf3fs_usrbio_sys::HF3FS_SUPER_MAGIC as _);
        }

        // 创建新Ior
        let final_ior = Ior::create(
            &self.mount_point,
            false,
            inflight.len() as i32,
            0,    // io_depth = 0
            1000,
            -1,
            0,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create final Ior: {}", e))?;

        // 重新prepare
        let final_inflight: Vec<_> = inflight.drain(..).collect();
        for (offset, size, buf_off) in &final_inflight {
            final_ior
                .prepare_raw(
                    iov,
                    *buf_off..buf_off + size,
                    fd,
                    *offset,
                    (*offset, *size),
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to prepare final I/O: offset={}, size={}, error={}",
                        offset,
                        size,
                        e
                    )
                })?;
        }

        if self.debug {
            if let Some(pb) = &self.progress_bar {
                pb.println(format!(
                    "[DEBUG] Submitting {} I/Os with new Ior (io_depth=0)",
                    final_inflight.len()
                ));
            }
        }

        final_ior.submit();
        let completed = final_ior.poll::<(usize, usize)>(1..=final_inflight.len(), 60000);

        if completed.is_empty() {
            return Err(anyhow::anyhow!(
                "Final batch poll timeout with {} I/Os",
                final_inflight.len()
            ));
        }

        for io in completed {
            if io.result < 0 {
                return Err(anyhow::anyhow!(
                    "Final write failed at offset {}: error {}",
                    io.extra.0,
                    io.result
                ));
            }
            if io.result as usize != io.extra.1 {
                return Err(anyhow::anyhow!(
                    "Final write incomplete: expected {}, got {}",
                    io.extra.1,
                    io.result
                ));
            }

            *completed_bytes += io.result as usize;
            self.stats_bytes
                .fetch_add(io.result as u64, Ordering::Relaxed);
            if let Some(pb) = &self.progress_bar {
                pb.inc(io.result as u64);
            }
        }

        Ok(())
    }

    /// 清理剩余inflight I/O
    fn drain_remaining_ios(
        &self,
        ior: &Ior,
        inflight: &mut VecDeque<(usize, usize, usize)>,
        completed_bytes: &mut usize,
    ) -> Result<()> {
        if self.debug {
            if let Some(pb) = &self.progress_bar {
                pb.println(format!(
                    "[DEBUG] Draining {} remaining in-flight I/Os",
                    inflight.len()
                ));
            }
        }

        let mut retry_count = 0;
        while !inflight.is_empty() && retry_count < 3 {
            retry_count += 1;

            let completed = ior.poll::<(usize, usize)>(1..=inflight.len(), 30000);

            for io in completed {
                if io.result < 0 {
                    return Err(anyhow::anyhow!(
                        "Final write failed at offset {}: error {}",
                        io.extra.0,
                        io.result
                    ));
                }
                *completed_bytes += io.result as usize;
                inflight.pop_front();

                self.stats_bytes
                    .fetch_add(io.result as u64, Ordering::Relaxed);
            }
        }

        if !inflight.is_empty() {
            return Err(anyhow::anyhow!(
                "Failed to drain {} I/Os after 3 attempts",
                inflight.len()
            ));
        }

        Ok(())
    }
}
