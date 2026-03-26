use std::{ffi::CString, path::Path};

/// 自动检测文件系统的块大小
#[allow(dead_code)]
pub fn get_filesystem_block_size(path: &Path) -> usize {
    // 尝试获取文件系统的块大小
    let path_str = match path.to_str() {
        Some(s) => s,
        None => return 1048576, // 默认 1MB
    };

    let c_path = match CString::new(path_str) {
        Ok(p) => p,
        Err(_) => return 1048576,
    };

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

    // 尝试 statvfs
    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };

    if result == 0 {
        let block_size = stat.f_bsize as usize;

        // 限制块大小在合理范围内 (4KB - 16MB)
        if block_size >= 4096 && block_size <= 16 * 1024 * 1024 {
            return block_size;
        }
    }

    // 如果 statvfs 失败，尝试父目录
    if let Some(parent) = path.parent() {
        if parent != path {
            return get_filesystem_block_size(parent);
        }
    }

    // 默认 1MB
    1048576
}
