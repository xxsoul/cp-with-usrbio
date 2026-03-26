use anyhow::{Context, Result};
use hf3fs_usrbio_sys::{Ior, Iov};
use indicatif::ProgressBar;
use shared_memory::ShmemConf;
use std::{
    collections::VecDeque,
    fs::File,
    io::Read,
    os::fd::{AsRawFd, IntoRawFd},
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
    shm_id: String,
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
                    "Worker {} failed to create shared memory: size={} bytes ({:.2} MB)",
                    worker_id, iov_size, iov_size as f64 / 1_000_000.0
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
            shm_id,
            mount_point: mount_point.clone(),
            size: iov_size,
        };

        if debug {
            if let Some(ref pb) = progress_bar {
                pb.println(format!(
                    "[Worker {}] Initialized with fixed shared memory: {:.2} MB",
                    worker_id,
                    iov_size as f64 / 1_000_000.0
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

        // 5. 同步文件长度
        unsafe {
            libc::ioctl(
                managed_fd.as_raw_fd(),
                hf3fs_usrbio_sys::HF3FS_SUPER_MAGIC as _,
            );
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
                }

                // 提交并等待完成
                final_ior.submit();
                let completed = final_ior.poll::<(usize, usize)>(1..=final_inflight.len(), 60000);

                if completed.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Final batch poll timeout with {} I/Os",
                        final_inflight.len()
                    ));
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
                    self.stats_bytes.fetch_add(io.result as u64, Ordering::Relaxed);
                    if let Some(ref pb) = self.progress_bar {
                        pb.inc(io.result as u64);
                    }
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
                    // 如果inflight为空，可以安全重置缓冲区位置
                    if inflight.is_empty() {
                        next_buf_offset = 0;
                        if self.debug {
                            if let Some(ref pb) = self.progress_bar {
                                pb.println(format!(
                                    "[Worker {}] Reset buffer offset for next batch",
                                    self.worker_id
                                ));
                            }
                        }
                    } else {
                        // 缓冲区不够，等待 inflight 完成
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
                let mut bytes_completed_this_round = 0;
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
                    bytes_completed_this_round += io.result as usize;
                    inflight.pop_front();

                    self.stats_bytes.fetch_add(io.result as u64, Ordering::Relaxed);
                    if let Some(ref pb) = self.progress_bar {
                        pb.inc(io.result as u64);
                    }
                }

                // 如果所有 inflight 都完成了，重置缓冲区位置
                if inflight.is_empty() {
                    next_buf_offset = 0;
                }
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
    while let Ok(task) = receiver.recv() {
        let relative_path = task.relative_path.clone();

        // 检查是否需要处理（断点续传）
        {
            let pm = progress_manager.lock().unwrap();
            if !pm.should_process(&relative_path) {
                continue;
            }
        }

        // 执行拷贝
        match ctx.copy_file(&task.src_path, &task.dst_path) {
            Ok(()) => {
                let mut pm = progress_manager.lock().unwrap();
                if let Err(e) = pm.mark_completed(relative_path) {
                    eprintln!("[Worker {}] Warning: Failed to mark completed: {}",
                              worker_id, e);
                }
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
