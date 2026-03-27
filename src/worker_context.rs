use anyhow::{Context, Result};
use hf3fs_usrbio_sys::{Ior, Iov};
use indicatif::ProgressBar;
use shared_memory::ShmemConf;
use std::{
    collections::VecDeque,
    fs::File,
    io::Read,
    os::fd::IntoRawFd,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use crate::managed_fd::ManagedFd;
use crate::progress::ProgressManager;
use crate::task::CopyTask;

/// Worker 线程上下文
///
/// 持有可复用的资源：iov、共享内存
pub struct WorkerContext {
    worker_id: usize,
    mount_point: String,
    block_size: usize,
    min_pipeline_depth: usize,
    max_pipeline_depth: usize,
    iov_size: usize,  // 固定大小，不再动态调整
    stats_bytes: std::sync::Arc<AtomicU64>,
    progress_bar: Option<ProgressBar>,
    preserve_attrs: bool,
    debug: bool,

    // 固定资源（初始化时创建，不再改变）
    shm_manager: ShmManager,
}

/// 共享内存管理器（简化版）
struct ShmManager {
    shm: shared_memory::Shmem,
    iov: Iov,
    mount_point: String,
    #[allow(dead_code)]
    size: usize,
}

impl Drop for ShmManager {
    fn drop(&mut self) {
        // 在drop iov之前，清理符号链接
        // Iov结构体中的id字段就是uuid (C char数组，需要转换)
        let iov_id_bytes: [u8; 16] = unsafe {
            std::mem::transmute(self.iov.0.id)
        };
        let iov_id = uuid::Uuid::from_bytes(iov_id_bytes);
        let symlink_path = format!(
            "{}/3fs-virt/iovs/{}",
            self.mount_point.trim_end_matches('/'),
            iov_id.as_hyphenated()
        );

        eprintln!("[DEBUG] ShmManager::Drop - Cleaning up symlink: {}", symlink_path);

        // 尝试删除符号链接
        if let Err(e) = std::fs::remove_file(&symlink_path) {
            eprintln!("[WARNING] Failed to remove symlink {}: {}", symlink_path, e);
        }

        // 然后iov和shm会自动drop
        // Rust会自动按字段声明逆序drop：
        // size → shm_id → iov (调用hf3fs_iovdestroy) → shm (释放共享内存)
        eprintln!("[DEBUG] ShmManager::Drop - iov and shm will be auto-dropped");
    }
}

impl WorkerContext {
    pub fn new(
        worker_id: usize,
        mount_point: String,
        block_size: usize,
        min_pipeline_depth: usize,
        max_pipeline_depth: usize,
        iov_size: usize,
        stats_bytes: std::sync::Arc<AtomicU64>,
        progress_bar: Option<ProgressBar>,
        preserve_attrs: bool,
        debug: bool,
    ) -> Result<Self> {
        // 立即创建固定大小的共享内存
        let shm_id = format!("cp_usrbio_w{}", worker_id);
        let shm = ShmemConf::new()
            .os_id(&shm_id)
            .size(iov_size)
            .create()
            .with_context(|| {
                format!(
                    "Worker {} failed to create shared memory: size={} bytes ({:.2} MiB)",
                    worker_id, iov_size, iov_size as f64 / 1_048_576.0
                )
            })?;

        // 创建iov（这会创建符号链接）
        let iov = Iov::wrap(&mount_point, &shm, -1).map_err(|e| {
            anyhow::anyhow!(
                "Worker {} failed to create Iov: {}",
                worker_id, e
            )
        })?;

        let shm_manager = ShmManager {
            shm,
            iov,
            mount_point: mount_point.clone(),
            size: iov_size,
        };

        if debug {
            if let Some(ref pb) = progress_bar {
                pb.println(format!(
                    "[Worker {}] Initialized with fixed shared memory: {:.2} MiB",
                    worker_id,
                    iov_size as f64 / 1_048_576.0
                ));
            }
        }

        Ok(Self {
            worker_id,
            mount_point,
            block_size,
            min_pipeline_depth,
            max_pipeline_depth,
            iov_size,
            stats_bytes,
            progress_bar,
            preserve_attrs,
            debug,
            shm_manager,
        })
    }

    /// 计算最优的pipeline深度（使用固定的iov_size）
    fn calculate_pipeline_depth(&self, file_size: u64) -> usize {
        let max_depth_by_shm = self.iov_size / self.block_size;
        if max_depth_by_shm == 0 {
            return 1;
        }

        let ideal_depth = if file_size < 10 * 1024 * 1024 {
            self.min_pipeline_depth.max(1)
        } else if file_size < 100 * 1024 * 1024 {
            let depth = (file_size / (20 * 1024 * 1024)) as usize + 1;
            depth.clamp(self.min_pipeline_depth, 4)
        } else if file_size < 1024 * 1024 * 1024 {
            let depth = (file_size / (200 * 1024 * 1024)) as usize * 2 + 2;
            depth.clamp(4, 8)
        } else {
            self.max_pipeline_depth.min(8)
        };

        ideal_depth
            .min(max_depth_by_shm)
            .min(self.max_pipeline_depth)
    }

    /// 拷贝单个文件
    pub fn copy_file(
        &mut self,
        src: &Path,
        dst: &Path,
    ) -> Result<()> {
        let start_time = Instant::now();

        if self.debug {
            if let Some(ref pb) = self.progress_bar {
                pb.println(format!(
                    "[Worker {}] Copying {:?} -> {:?}",
                    self.worker_id, src, dst
                ));
            }
        }

        // 1. 打开源文件
        let mut src_file = File::open(src)
            .with_context(|| format!("Failed to open source: {:?}", src))?;

        let metadata = src_file.metadata()
            .with_context(|| format!("Failed to get metadata: {:?}", src))?;

        let file_size = metadata.len();

        // 空文件特殊处理
        if file_size == 0 {
            File::create(dst)
                .with_context(|| format!("Failed to create: {:?}", dst))?;
            return Ok(());
        }

        // 2. 创建目标文件并注册FD
        let dst_file = File::create(dst)
            .with_context(|| format!("Failed to create target: {:?}", dst))?;

        let raw_fd = dst_file.into_raw_fd();
        let managed_fd = unsafe { ManagedFd::from_raw_fd(raw_fd) };

        // 3. 获取 iov 和 shm_ptr（固定大小，不再调整）
        let iov_ptr = &self.shm_manager.iov as *const Iov;
        let shm_ptr = self.shm_manager.shm.as_ptr();

        // 4. 执行 Pipeline 拷贝
        let pipeline_depth = self.calculate_pipeline_depth(file_size);

        // 创建 Ior（每个文件创建新的，因为需要不同的 pipeline_depth）
        let ior = Ior::create(
            &self.mount_point,
            false,
            pipeline_depth as i32,
            -(pipeline_depth as i32),
            1000,
            -1,
            0,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create Ior: {}", e))?;

        // 执行 pipeline
        self.execute_pipeline(
            &mut src_file,
            managed_fd.as_raw_fd(),
            unsafe { &*iov_ptr },
            &ior,
            file_size,
            shm_ptr,
            pipeline_depth,
        )?;

        // 确保Ior中的所有请求都已完成
        // 这是额外的安全检查，防止在ioctl时还有未完成的I/O
        if self.debug {
            if let Some(ref pb) = self.progress_bar {
                pb.println(format!(
                    "[Worker {}] All I/O completed, syncing to disk...",
                    self.worker_id
                ));
            }
        }

        // 5. 同步文件长度（确保所有数据持久化）
        unsafe {
            libc::ioctl(
                managed_fd.as_raw_fd(),
                hf3fs_usrbio_sys::HF3FS_SUPER_MAGIC as _,
            );
        }

        // 额外同步：确保所有I/O操作完成后再关闭文件
        // 这是为了避免缓冲区被下一个文件重用时覆盖未完成的I/O
        unsafe {
            libc::fsync(managed_fd.as_raw_fd());
        }

        // 6. 保留文件属性
        if self.preserve_attrs {
            std::fs::set_permissions(dst, metadata.permissions())
                .with_context(|| format!("Failed to set permissions: {:?}", dst))?;
        }

        if let Some(ref pb) = self.progress_bar {
            pb.println(format!(
                "✓ [Worker {}] {:?} ({:.2}s)",
                self.worker_id,
                src,
                start_time.elapsed().as_secs_f64()
            ));
        }

        Ok(())
    }

    /// 执行 Pipeline 拷贝（简化版，保留核心逻辑）
    fn execute_pipeline(
        &mut self,
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
        let mut next_buf_offset: usize = 0;  // 下一个可用的缓冲区位置

        // 记录已使用的最大缓冲区位置，用于检测缓冲区使用情况
        let mut max_buf_offset_used: usize = 0;

        while completed_bytes < file_size as usize {
            // 检查是否是最后一批不足的情况
            let remaining_bytes = file_size as usize - src_offset;
            let remaining_ios = (remaining_bytes + self.block_size - 1) / self.block_size;
            let is_potential_final_batch = remaining_ios < pipeline_depth && remaining_ios > 0;

            // 如果是最后一批不足，直接用final_ior处理，不prepare到ior
            if is_potential_final_batch && inflight.is_empty() {
                if self.debug {
                    if let Some(ref pb) = self.progress_bar {
                        pb.println(format!(
                            "[Worker {}] Handling final batch with {} I/Os (partial batch)",
                            self.worker_id, remaining_ios
                        ));
                    }
                }

                // 读取所有剩余数据并prepare到final_ior
                let final_ior = Ior::create(
                    &self.mount_point,
                    false,
                    remaining_ios as i32,
                    0,    // io_depth = 0（非批处理模式）
                    1000,
                    -1,
                    0,
                ).map_err(|e| anyhow::anyhow!("Failed to create final Ior: {}", e))?;

                let mut final_inflight = Vec::new();

                while src_offset < file_size as usize {
                    let chunk_size = std::cmp::min(self.block_size, file_size as usize - src_offset);

                    // 检查缓冲区空间
                    if next_buf_offset + chunk_size > self.iov_size {
                        if inflight.is_empty() {
                            next_buf_offset = 0;
                        } else {
                            return Err(anyhow::anyhow!(
                                "Insufficient buffer space for final batch"
                            ));
                        }
                    }

                    let buf_offset = next_buf_offset;

                    // 读取数据
                    unsafe {
                        let buf_slice = std::slice::from_raw_parts_mut(
                            shm_ptr.add(buf_offset),
                            chunk_size,
                        );
                        src_file.read_exact(buf_slice)
                            .with_context(|| "Failed to read from source")?;
                    }

                    // Prepare到final_ior
                    final_ior.prepare_raw(
                        iov,
                        buf_offset..buf_offset + chunk_size,
                        fd,
                        src_offset,
                        (src_offset, chunk_size),
                    ).map_err(|e| {
                        anyhow::anyhow!("Failed to prepare final I/O: offset={}, size={}, error={}",
                            src_offset, chunk_size, e)
                    })?;

                    final_inflight.push((src_offset, chunk_size, buf_offset));
                    src_offset += chunk_size;
                    next_buf_offset += chunk_size;
                    max_buf_offset_used = max_buf_offset_used.max(next_buf_offset);
                }

                // 提交并等待完成
                final_ior.submit();

                // 循环poll直到所有I/O完成
                let mut final_completed_count = 0;
                let mut poll_attempts = 0;
                const MAX_POLL_ATTEMPTS: usize = 10;

                while final_completed_count < final_inflight.len() && poll_attempts < MAX_POLL_ATTEMPTS {
                    let completed = final_ior.poll::<(usize, usize)>(1..=final_inflight.len() - final_completed_count, 60000);

                    if completed.is_empty() {
                        poll_attempts += 1;
                        if self.debug {
                            if let Some(ref pb) = self.progress_bar {
                                pb.println(format!(
                                    "[Worker {}] Final batch poll attempt {} returned empty, {}/{} completed",
                                    self.worker_id, poll_attempts, final_completed_count, final_inflight.len()
                                ));
                            }
                        }
                        continue;
                    }

                    // 处理完成的I/O
                    for io in completed {
                        if io.result < 0 {
                            return Err(anyhow::anyhow!(
                                "Final write failed at offset {}: error {}",
                                io.extra.0, io.result
                            ));
                        }
                        if io.result as usize != io.extra.1 {
                            return Err(anyhow::anyhow!(
                                "Final write incomplete: expected {}, got {}",
                                io.extra.1, io.result
                            ));
                        }

                        completed_bytes += io.result as usize;
                        final_completed_count += 1;
                        self.stats_bytes.fetch_add(io.result as u64, Ordering::Relaxed);
                        if let Some(ref pb) = self.progress_bar {
                            pb.inc(io.result as u64);
                        }
                    }
                }

                if final_completed_count < final_inflight.len() {
                    return Err(anyhow::anyhow!(
                        "Final batch incomplete: only {}/{} I/Os completed after {} attempts",
                        final_completed_count, final_inflight.len(), poll_attempts
                    ));
                }

                // 同步所有写入，确保数据持久化
                unsafe {
                    libc::ioctl(fd, hf3fs_usrbio_sys::HF3FS_SUPER_MAGIC as _);
                }

                // 最后一批处理完成，退出循环
                break;
            }

            // 阶段1: 准备 I/O（正常批次）
            while inflight.len() < pipeline_depth && src_offset < file_size as usize {
                let chunk_size = std::cmp::min(self.block_size, file_size as usize - src_offset);

                // 检查是否有足够的缓冲区空间
                if next_buf_offset + chunk_size > self.iov_size {
                    // 缓冲区不够用
                    if inflight.is_empty() {
                        // 当前没有正在进行的I/O，可以安全重置缓冲区位置
                        // 但必须先确保之前的所有I/O已经真正完成
                        // 由于我们在poll成功后才pop，此时应该已经安全
                        if self.debug {
                            if let Some(ref pb) = self.progress_bar {
                                pb.println(format!(
                                    "[Worker {}] Buffer wrap-around: resetting from {} to 0",
                                    self.worker_id, next_buf_offset
                                ));
                            }
                        }
                        next_buf_offset = 0;
                    } else {
                        // 还有I/O在进行，不能重置，需要等待
                        break;
                    }
                }

                let buf_offset = next_buf_offset;

                // 额外检查：确保chunk_size不超过iov_size
                if chunk_size > self.iov_size {
                    return Err(anyhow::anyhow!(
                        "Chunk size {} exceeds iov_size {}. This should not happen (block_size = {})",
                        chunk_size, self.iov_size, self.block_size
                    ));
                }

                // 读取数据
                unsafe {
                    let buf_slice = std::slice::from_raw_parts_mut(
                        shm_ptr.add(buf_offset),
                        chunk_size,
                    );
                    src_file.read_exact(buf_slice)
                        .with_context(|| "Failed to read from source")?;
                }

                // 准备 I/O
                ior.prepare_raw(
                    iov,
                    buf_offset..buf_offset + chunk_size,
                    fd,
                    src_offset,
                    (src_offset, chunk_size),
                )
                .map_err(|e| anyhow::anyhow!("Failed to prepare I/O: {}", e))?;

                inflight.push_back((src_offset, chunk_size, buf_offset));
                src_offset += chunk_size;
                next_buf_offset += chunk_size;
                max_buf_offset_used = max_buf_offset_used.max(next_buf_offset);

                // 如果缓冲区用完，等待 inflight 完成
                if next_buf_offset >= self.iov_size {
                    break;
                }
            }

            // 阶段2: 提交并等待完成
            if !inflight.is_empty() {
                ior.submit();

                let max_poll = std::cmp::min(inflight.len(), pipeline_depth);
                let completed = ior.poll::<(usize, usize)>(1..=max_poll, 30000);

                if completed.is_empty() && !inflight.is_empty() {
                    if self.debug {
                        if let Some(ref pb) = self.progress_bar {
                            pb.println(format!(
                                "[Worker {}] Poll timeout, inflight: {:?}",
                                self.worker_id, inflight
                            ));
                        }
                    }
                    return Err(anyhow::anyhow!("Poll timeout after 30s"));
                }

                // 处理完成的 I/O
                for io in completed {
                    if io.result < 0 {
                        return Err(anyhow::anyhow!(
                            "Write failed at offset {}: error {}",
                            io.extra.0, io.result
                        ));
                    }
                    if io.result as usize != io.extra.1 {
                        return Err(anyhow::anyhow!(
                            "Write incomplete: expected {}, got {}",
                            io.extra.1, io.result
                        ));
                    }

                    completed_bytes += io.result as usize;
                    inflight.pop_front();

                    self.stats_bytes.fetch_add(io.result as u64, Ordering::Relaxed);
                    if let Some(ref pb) = self.progress_bar {
                        pb.inc(io.result as u64);
                    }
                }

                // 重要：不要立即重置缓冲区！
                // 即使 inflight 为空，也要等待当前批次完全完成
                // 重置逻辑已移至循环开始时检查
            }
        }

        // 文件结束时，确保所有I/O真正完成
        // 不需要重置缓冲区，因为下一个文件会重新开始

        if self.debug {
            if let Some(ref pb) = self.progress_bar {
                pb.println(format!(
                    "[Worker {}] File completed, max buffer usage: {:.2} MiB / {:.2} MiB",
                    self.worker_id,
                    max_buf_offset_used as f64 / 1_048_576.0,
                    self.iov_size as f64 / 1_048_576.0
                ));
            }
        }

        // 最终验证
        if completed_bytes != file_size as usize {
            return Err(anyhow::anyhow!(
                "Size mismatch: completed {} != expected {}",
                completed_bytes, file_size
            ));
        }

        Ok(())
    }
}

/// Worker 线程主循环
pub fn worker_thread(
    worker_id: usize,
    receiver: crossbeam::channel::Receiver<CopyTask>,
    mount_point: String,
    block_size: usize,
    min_pipeline_depth: usize,
    max_pipeline_depth: usize,
    iov_size: usize,
    stats_bytes: std::sync::Arc<AtomicU64>,
    progress_bar: Option<ProgressBar>,
    preserve_attrs: bool,
    debug: bool,
    progress_manager: std::sync::Arc<std::sync::Mutex<ProgressManager>>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // 创建 worker 上下文（固定大小的共享内存）
    let mut ctx = match WorkerContext::new(
        worker_id,
        mount_point,
        block_size,
        min_pipeline_depth,
        max_pipeline_depth,
        iov_size,
        stats_bytes,
        progress_bar.clone(),
        preserve_attrs,
        debug,
    ) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("[Worker {}] Failed to initialize: {}", worker_id, e);
            return;
        }
    };

    // 处理任务
    while running.load(std::sync::atomic::Ordering::SeqCst) {
        // 使用 try_recv 避免阻塞，以便检查 running 标志
        let task = match receiver.try_recv() {
            Ok(task) => task,
            Err(crossbeam::channel::TryRecvError::Empty) => {
                // 通道为空，短暂休眠后继续检查
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            Err(crossbeam::channel::TryRecvError::Disconnected) => {
                // 通道已关闭，退出
                break;
            }
        };

        let relative_path = task.relative_path.clone();

        // 检查是否需要处理（断点续传）
        {
            let pm = progress_manager.lock().unwrap();
            if !pm.should_process(&relative_path) {
                continue;
            }
        }

        // 检查是否收到中断信号
        if !running.load(std::sync::atomic::Ordering::SeqCst) {
            eprintln!("[Worker {}] Received shutdown signal, stopping...", worker_id);
            break;
        }

        // 执行拷贝
        match ctx.copy_file(&task.src_path, &task.dst_path) {
            Ok(()) => {
                let mut pm = progress_manager.lock().unwrap();
                if let Err(e) = pm.mark_completed(relative_path) {
                    eprintln!("[Worker {}] Warning: Failed to mark completed: {}",
                              worker_id, e);
                }
                // 从失败列表中移除（如果是重试成功的任务）
                pm.remove_from_failed(&task.relative_path);
            }
            Err(e) => {
                let mut pm = progress_manager.lock().unwrap();
                if let Err(e) = pm.mark_failed(relative_path) {
                    eprintln!("[Worker {}] Warning: Failed to mark failed: {}",
                              worker_id, e);
                }

                if let Some(ref pb) = ctx.progress_bar {
                    pb.println(format!(
                        "✗ [Worker {}] Failed to copy {:?}: {}",
                        worker_id, task.src_path, e
                    ));
                }
            }
        }
    }

    // Worker 结束时，资源自动释放
    if debug {
        eprintln!("[Worker {}] Shutting down, releasing resources", worker_id);
    }
}
