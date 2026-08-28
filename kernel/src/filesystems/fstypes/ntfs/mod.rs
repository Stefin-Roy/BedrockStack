pub mod attr;
pub mod boot;
pub mod index;
pub mod inode;
pub mod io;
pub mod mount;
pub mod record;
pub mod runs;

pub use inode::NtfsInode;
pub use mount::NtfsFileSystem;
pub use mount::NtfsSuperBlock;

#[cfg(feature = "selftest")]
pub mod selftest;

/// Last detailed NTFS mount error for diagnostics (sector read vs usa_fixup
/// vs MFT FILE miss). Stored as `&'static str` so it survives the
/// `VfsError::IOError` collapse and can be surfaced in `lib.rs:577`.
static LAST_NTFS_DETAIL: crate::filesystems::vfs::irq::IrqMutex<Option<&'static str>> = crate::filesystems::vfs::irq::IrqMutex::new(None);

pub fn last_error() -> Option<&'static str> {
    *LAST_NTFS_DETAIL.lock()
}

pub(crate) fn set_last_error(s: Option<&'static str>) {
    *LAST_NTFS_DETAIL.lock() = s;
}

pub(crate) fn set_last_error_if_none(s: &'static str) {
    let mut g = LAST_NTFS_DETAIL.lock();
    if g.is_none() {
        *g = Some(s);
    }
}
