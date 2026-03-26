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
/// 持有可复用的资源：iov、ior、共享内存
pub struct WorkerContext {
    worker_id: usize,
    mount_point: String,
    block_size: usize,
    min_pipeline_depth: usize,
    max_pipeline_depth: usize,
    max_iov_size: usize,
    stats_bytes: std::sync::Arc<AtomicU64>,
    progress_bar: Option<ProgressBar>,
    preserve_attrs: bool,
    debug: bool,

    // 复用的资源
    shm_manager: Option<ShmManager>,
    current_iov_size: usize,
}

/// 共享内存管理器（简化版）
struct ShmManager {
    #[allow(dead_code)]
    shm: shared_memory::Shmem,
    iov: Iov,
    #[allow(dead_code)]
    shm_id: String,
    size: usize,
}

impl WorkerContext {
    pub fn new(
        worker_id: usize,
        mount_point: String,
        block_size: usize,
        min_pipeline_depth: usize,
        max_pipeline_depth: usize,
        max_iov_size: usize,
        stats_bytes: std::sync::Arc<AtomicU64>,
        progress_bar: Option<ProgressBar>,
        preserve_attrs: bool,
        debug: bool,
    ) -> Self {
        Self {
            worker_id,
            mount_point,
            block_size,
            min_pipeline_depth,
            max_pipeline_depth,
            max_iov_size,
            stats_bytes,
            progress_bar,
            preserve_attrs,
            debug,
            shm_manager: None,
            current_iov_size: 0,
        }
    }

    /// 确保共享内存足够大
    fn ensure_shm_capacity(&mut self, required_size: usize) -> Result<()> {
        // 如果已存在且大小足够，直接返回
        if let Some(ref shm) = self.shm_manager {
            if shm.size >= required_size {
                return Ok(());
            }
        }

        // 需要重新创建更大的共享内存
        if self.debug {
            if let Some(ref pb) = self.progress_bar {
                pb.println(format!(
                    "[Worker {}] Resizing shared memory: {} -> {} bytes",
                    self.worker_id, self.current_iov_size, required_size
                ));
            }
        }

        // 销毁旧的
        self.shm_manager = None;

        // 创建新的
        let shm_id = format!("cp_usrbio_w{}", self.worker_id);
        let shm = ShmemConf::new()
            .os_id(&shm_id)
            .size(required_size)
            .create()
            .with_context(|| {
                format!(
                    "Worker {} failed to create shared memory: size={}",
                    self.worker_id, required_size
                )
            })?;

        let iov = Iov::wrap(&self.mount_point, &shm, -1).map_err(|e| {
            anyhow::anyhow!(
                "Worker {} failed to create Iov: {}",
                self.worker_id, e
            )
        })?;

        self.shm_manager = Some(ShmManager {
            shm,
            iov,
            shm_id,
            size: required_size,
        });
        self.current_iov_size = required_size;

        Ok(())
    }

    /// 计算实际需要的共享内存大小
    fn calculate_iov_size(&self, file_size: u64) -> usize {
        let size = file_size as usize;
        let min_size = self.block_size;
        size.max(min_size).min(self.max_iov_size)
    }

    /// 计算最优的pipeline深度
    fn calculate_pipeline_depth(&self, file_size: u64, iov_size: usize) -> usize {
        let max_depth_by_shm = iov_size / self.block_size;
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

        // 3. 确保共享内存足够
        let iov_size = self.calculate_iov_size(file_size);
        self.ensure_shm_capacity(iov_size)?;

        // 提取 iov 和 shm_ptr（避免借用冲突）
        let (iov_ptr, shm_ptr) = {
            let shm_manager = self.shm_manager.as_ref().unwrap();
            let iov_ptr = &shm_manager.iov as *const Iov;
            let shm_ptr = shm_manager.shm.as_ptr();
            (iov_ptr, shm_ptr)
        };

        // 4. 执行 Pipeline 拷贝
        let pipeline_depth = self.calculate_pipeline_depth(file_size, iov_size);

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
        let mut buf_offset_counter: usize = 0;

        while completed_bytes < file_size as usize {
            // 阶段1: 准备 I/O
            while inflight.len() < pipeline_depth && src_offset < file_size as usize {
                let buf_offset = (buf_offset_counter * self.block_size) % self.current_iov_size;
                let chunk_size = std::cmp::min(self.block_size, file_size as usize - src_offset);

                if buf_offset + chunk_size > self.current_iov_size {
                    break;
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
                buf_offset_counter += 1;
            }

            // 阶段2: 提交并等待完成
            if !inflight.is_empty() {
                ior.submit();

                let max_poll = std::cmp::min(inflight.len(), pipeline_depth);
                let completed = ior.poll::<(usize, usize)>(1..=max_poll, 30000);

                if completed.is_empty() && !inflight.is_empty() {
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
    max_iov_size: usize,
    stats_bytes: std::sync::Arc<AtomicU64>,
    progress_bar: Option<ProgressBar>,
    preserve_attrs: bool,
    debug: bool,
    progress_manager: std::sync::Arc<std::sync::Mutex<ProgressManager>>,
) {
    // 创建 worker 上下文（复用资源）
    let mut ctx = WorkerContext::new(
        worker_id,
        mount_point,
        block_size,
        min_pipeline_depth,
        max_pipeline_depth,
        max_iov_size,
        stats_bytes,
        progress_bar,
        preserve_attrs,
        debug,
    );

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
