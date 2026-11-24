//! Linux 上的错误码。名字和 Windows 上的 NTSTATUS 一样（两边共用的那部分按这些名字写），值是
//! errno，FUSE 那边直接交给内核，读写的程序拿到的就是它。

#[allow(clippy::upper_case_acronyms)]
pub type NTSTATUS = i32;

/// 不是 errno：读到了文件末尾。FUSE 那边答一段空数据
pub const STATUS_END_OF_FILE: NTSTATUS = -1;
pub const STATUS_ACCESS_DENIED: NTSTATUS = libc::EACCES;
pub const STATUS_OBJECT_NAME_INVALID: NTSTATUS = libc::EINVAL;
pub const STATUS_SHARING_VIOLATION: NTSTATUS = libc::ETXTBSY;
pub const STATUS_DISK_FULL: NTSTATUS = libc::ENOSPC;
pub const STATUS_NOT_A_DIRECTORY: NTSTATUS = libc::ENOTDIR;
/// 和主机的连接断了：程序看到的是「Transport endpoint is not connected」，和卸载了一半的 FUSE 一样
pub const STATUS_CONNECTION_DISCONNECTED: NTSTATUS = libc::ENOTCONN;
pub const STATUS_MEDIA_WRITE_PROTECTED: NTSTATUS = libc::EROFS;
pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = libc::ENOENT;
pub const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = libc::EEXIST;
pub const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = libc::EISDIR;
pub const STATUS_UNEXPECTED_IO_ERROR: NTSTATUS = libc::EIO;
pub const STATUS_IO_TIMEOUT: NTSTATUS = libc::ETIMEDOUT;
pub const STATUS_DIRECTORY_NOT_EMPTY: NTSTATUS = libc::ENOTEMPTY;
/// 跨了主机上的文件系统（改名挪不过去）：`mv` 见到它会改成复制再删
pub const STATUS_NOT_SAME_DEVICE: NTSTATUS = libc::EXDEV;
/// 只有 root 能做的（改属主、改组）
pub const STATUS_PRIVILEGE_NOT_HELD: NTSTATUS = libc::EPERM;
