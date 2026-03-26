use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// 管理已注册的文件描述符
///
/// 这个结构体封装了3FS文件描述符的注册和注销逻辑。
/// 当创建时会自动注册fd，当销毁时会自动注销fd。
pub struct ManagedFd {
    fd: OwnedFd,
}

impl ManagedFd {
    /// 从已打开的文件创建ManagedFd
    ///
    /// # Safety
    /// 调用者必须确保传入的fd是有效的且尚未注册
    pub unsafe fn from_raw_fd(raw_fd: i32) -> Self {
        // 注册fd到3FS
        hf3fs_usrbio_sys::hf3fs_reg_fd(raw_fd, 0);

        // 包装为OwnedFd
        let owned_fd = OwnedFd::from_raw_fd(raw_fd);

        Self { fd: owned_fd }
    }

    /// 获取原始文件描述符
    pub fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

impl Drop for ManagedFd {
    fn drop(&mut self) {
        // 自动注销fd
        unsafe {
            hf3fs_usrbio_sys::hf3fs_dereg_fd(self.fd.as_raw_fd());
        }
    }
}
