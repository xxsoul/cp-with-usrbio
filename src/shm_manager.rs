use anyhow::{Context, Result};
use hf3fs_usrbio_sys::Iov;
use shared_memory::ShmemConf;
use std::backtrace::Backtrace;
use std::fs;

/// 获取/dev/shm的可用空间（字节）
pub fn get_shm_available_space() -> Result<u64> {
    // 使用statvfs获取文件系统统计信息
    let mut statvfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let c_path = std::ffi::CString::new("/dev/shm").context("Invalid path")?;

    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut statvfs) };
    if result != 0 {
        return Err(anyhow::anyhow!(
            "Failed to get /dev/shm filesystem stats"
        ));
    }

    // 计算可用空间：块大小 * 可用块数
    let available = statvfs.f_bsize as u64 * statvfs.f_bavail as u64;
    Ok(available)
}

/// 获取/dev/shm的总大小（字节）
pub fn get_total_shm_size() -> Result<u64> {
    let mut statvfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let c_path = std::ffi::CString::new("/dev/shm").context("Invalid path")?;

    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut statvfs) };
    if result != 0 {
        return Err(anyhow::anyhow!(
            "Failed to get /dev/shm filesystem stats"
        ));
    }

    // 计算总大小：块大小 * 总块数
    let total = statvfs.f_blocks as u64 * statvfs.f_bsize as u64;
    Ok(total)
}

/// 获取/dev/shm的总大小和已用空间
fn get_shm_usage() -> Result<(u64, u64)> {
    let mut statvfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let c_path = std::ffi::CString::new("/dev/shm").context("Invalid path")?;

    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut statvfs) };
    if result != 0 {
        return Err(anyhow::anyhow!(
            "Failed to get /dev/shm filesystem stats"
        ));
    }

    let total = statvfs.f_blocks as u64 * statvfs.f_bsize as u64;
    let available = statvfs.f_bsize as u64 * statvfs.f_bavail as u64;
    let used = total - available;

    Ok((total, used))
}

/// 检查共享内存空间是否足够
///
/// # 参数
/// - required_size: 需要的共享内存大小（字节）
/// - safety_margin: 安全裕度比例（例如 0.2 表示需要20%的额外空间）
///
/// # 返回
/// - Ok(()) 如果空间足够
/// - Err 包含详细信息的错误
#[allow(dead_code)]
fn check_shm_space(required_size: usize, safety_margin: f64) -> Result<()> {
    let available = get_shm_available_space()?;
    let (total, used) = get_shm_usage()?;

    // 加上安全裕度
    let required_with_margin = (required_size as f64 * (1.0 + safety_margin)) as u64;

    if available < required_with_margin {
        let available_mb = available as f64 / 1_048_576.0;
        let required_mb = required_size as f64 / 1_048_576.0;
        let total_mb = total as f64 / 1_048_576.0;
        let used_mb = used as f64 / 1_048_576.0;
        let used_percent = (used as f64 / total as f64) * 100.0;

        return Err(anyhow::anyhow!(
            "Insufficient /dev/shm space!\n\
             ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
             📊 /dev/shm Status:\n\
             ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
               Total:      {:>8.2} MiB\n\
               Used:       {:>8.2} MiB ({:.1}%)\n\
               Available:  {:>8.2} MiB\n\
             \n\
             🎯 Required: {:>8.2} MiB (with {:.0}% safety margin)\n\
             ❌ Shortage: {:>8.2} MiB\n\
             ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
             \n\
             💡 Solutions:\n\
             \n\
             1. Reduce concurrent workers:\n\
                --workers 2\n\
             \n\
             2. Reduce max shared memory per worker:\n\
                --max-iov-size 67108864  # 64MiB\n\
             \n\
             3. Clean up /dev/shm:\n\
                rm -f /dev/shm/cp_usrbio_*\n\
                # or clean all:\n\
                sudo rm -rf /dev/shm/*\n\
             \n\
             4. Increase /dev/shm size:\n\
                sudo mount -o remount,size=4G /dev/shm\n\
             \n\
             5. Wait for other processes to finish\n",
            total_mb,
            used_mb,
            used_percent,
            available_mb,
            required_mb,
            safety_margin * 100.0,
            (required_with_margin - available) as f64 / 1_048_576.0
        ));
    }

    Ok(())
}

/// 共享内存和Iov管理器
///
/// 封装了共享内存的创建和Iov的包装逻辑
pub struct ShmManager {
    #[allow(dead_code)]
    shm: shared_memory::Shmem,
    #[allow(dead_code)]
    pub iov: Iov,
    #[allow(dead_code)]
    pub shm_id: String,
}

impl ShmManager {
    /// 创建新的共享内存管理器
    ///
    /// # 参数
    /// - mount_point: 3FS挂载点
    /// - iov_size: 共享内存大小（字节）
    /// - debug: 是否启用debug模式
    /// - progress_bar: 可选的进度条（用于debug输出）
    #[allow(dead_code)]
    pub fn new(
        mount_point: &str,
        iov_size: usize,
        debug: bool,
        progress_bar: Option<&indicatif::ProgressBar>,
    ) -> Result<Self> {
        // 检查共享内存空间（使用20%的安全裕度）
        if let Err(e) = check_shm_space(iov_size, 0.2) {
            eprintln!("{}", e);
            return Err(e);
        }

        // 生成唯一的共享内存ID
        let shm_id = format!("cp_usrbio_{}", uuid::Uuid::new_v4().simple());

        if debug {
            if let Some(pb) = progress_bar {
                pb.println(format!(
                    "[DEBUG] Creating shared memory: id={}, size={}",
                    shm_id, iov_size
                ));
            }
        }

        // 创建共享内存
        let shm = ShmemConf::new()
            .os_id(&shm_id)
            .size(iov_size)
            .create()
            .with_context(|| {
                let backtrace = Backtrace::capture();

                // 获取更详细的错误信息
                let available = get_shm_available_space().unwrap_or(0);
                let (total, used) = get_shm_usage().unwrap_or((0, 0));

                format!(
                    "Failed to create shared memory\n\
                     [DEBUG] Shared memory ID: {}\n\
                     [DEBUG] Requested size: {} bytes ({:.2} MiB)\n\
                     [DEBUG] /dev/shm Total: {:.2} MiB\n\
                     [DEBUG] /dev/shm Used: {:.2} MiB\n\
                     [DEBUG] /dev/shm Available: {:.2} MiB\n\
                     \n\
                     💡 This might be due to:\n\
                     1. Insufficient /dev/shm space (see above)\n\
                     2. Too many concurrent workers\n\
                     3. Other processes using shared memory\n\
                     \n\
                     Try: --workers 2 --max-iov-size 67108864\n\
                     \n\
                     Backtrace:\n{}",
                    shm_id,
                    iov_size,
                    iov_size as f64 / 1_048_576.0,
                    total as f64 / 1_048_576.0,
                    used as f64 / 1_048_576.0,
                    available as f64 / 1_048_576.0,
                    backtrace
                )
            })?;

        // 打印共享内存详情
        if debug {
            if let Some(pb) = progress_bar {
                pb.println("[DEBUG] Shared memory created successfully");
                pb.println(format!("[DEBUG]   OS ID: {:?}", shm.get_os_id()));
                pb.println(format!("[DEBUG]   Pointer: {:?}", shm.as_ptr()));
                pb.println(format!("[DEBUG]   Size: {}", shm.len()));
                pb.println(format!("[DEBUG]   Owner: {}", shm.is_owner()));
            }
        }

        // 创建Iov
        let os_id = shm.get_os_id();
        let targ = if os_id.starts_with('/') {
            format!("/dev/shm{}", os_id)
        } else {
            format!("/dev/shm/{}", os_id)
        };

        if debug {
            if let Some(pb) = progress_bar {
                let iov_uuid = uuid::Uuid::new_v4();
                let link = format!(
                    "{}/3fs-virt/iovs/{}",
                    mount_point.trim_end_matches('/'),
                    iov_uuid.as_hyphenated()
                );
                pb.println("[DEBUG] Attempting to create symlink:");
                pb.println(format!("[DEBUG]   Target: {}", targ));
                pb.println(format!("[DEBUG]   Link: {}", link));
                pb.println(format!(
                    "[DEBUG]   Target exists: {}",
                    std::path::Path::new(&targ).exists()
                ));
            }
        }

        let iov = Iov::wrap(mount_point, &shm, -1).map_err(|e| {
            let backtrace = Backtrace::capture();
            anyhow::anyhow!(
                "Failed to create Iov: {}\n\
                 [DEBUG] Mount point: {}\n\
                 [DEBUG] Shared memory ID: {}\n\
                 [DEBUG] Shared memory OS ID: {:?}\n\
                 [DEBUG] Shared memory size: {} bytes\n\
                 [DEBUG] Shared memory pointer: {:?}\n\
                 [DEBUG] NUMA node: -1\n\
                 [DEBUG] Target symlink path: {}\n\
                 [DEBUG] Error code: {}\n\
                 Backtrace:\n{}",
                e,
                mount_point,
                shm_id,
                os_id,
                iov_size,
                shm.as_ptr(),
                targ,
                e,
                backtrace
            )
        })?;

        Ok(Self {
            shm,
            iov,
            shm_id,
        })
    }

    /// 获取共享内存指针
    #[allow(dead_code)]
    pub fn as_ptr(&self) -> *mut u8 {
        self.shm.as_ptr()
    }

    /// 获取共享内存大小
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.shm.len()
    }
}

/// 清理所有遗留的共享内存文件
pub fn cleanup_stale_shm() -> Result<usize> {
    let mut cleaned = 0;
    let mut failed = 0;

    if let Ok(entries) = fs::read_dir("/dev/shm") {
        for entry in entries.filter_map(|e| e.ok()) {
            if let Ok(name) = entry.file_name().into_string() {
                // 清理我们创建的共享内存文件
                if name.starts_with("cp_usrbio_") || name.starts_with("cp_usrbio.") {
                    let path = entry.path();
                    match fs::remove_file(&path) {
                        Ok(_) => {
                            eprintln!("  Cleaned: {}", name);
                            cleaned += 1;
                        }
                        Err(e) => {
                            eprintln!("  Failed to clean {}: {}", name, e);
                            failed += 1;
                        }
                    }
                }
            }
        }
    }

    if failed > 0 {
        eprintln!("Warning: Failed to clean {} files (may need root permissions)", failed);
    }

    Ok(cleaned)
}

/// 显示当前共享内存使用情况
pub fn show_shm_status() -> Result<()> {
    let (total, used) = get_shm_usage()?;
    let available = get_shm_available_space()?;

    let total_mb = total as f64 / 1_048_576.0;
    let used_mb = used as f64 / 1_048_576.0;
    let available_mb = available as f64 / 1_048_576.0;
    let used_percent = if total > 0 {
        (used as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("📊 /dev/shm Memory Status");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Total:      {:>8.2} MiB", total_mb);
    println!("Used:       {:>8.2} MiB ({:.1}%)", used_mb, used_percent);
    println!("Available:  {:>8.2} MiB", available_mb);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // 列出可能相关的文件
    if let Ok(entries) = fs::read_dir("/dev/shm") {
        let mut our_files = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            if let Ok(name) = entry.file_name().into_string() {
                if name.starts_with("cp_usrbio") {
                    if let Ok(metadata) = entry.metadata() {
                        our_files.push((name, metadata.len()));
                    }
                }
            }
        }

        if !our_files.is_empty() {
            println!("\n⚠️  Found {} cp-with-usrbio shared memory files:", our_files.len());
            let total_ours: u64 = our_files.iter().map(|(_, size)| *size).sum();
            for (name, size) in our_files {
                println!("  {} ({:.2} MiB)", name, size as f64 / 1_048_576.0);
            }
            println!(
                "Total: {:.2} MiB",
                total_ours as f64 / 1_048_576.0
            );
            println!("\n💡 Run with --shm-status to clean them up");
        }
    }

    Ok(())
}
