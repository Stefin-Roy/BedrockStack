//! Synthetic "everything is a file" trees.
//!
//! These are in-memory `FileOps` directories that expose kernel services,
//! devices, tasks, and control points as files.  They are not backed by a
//! disk filesystem; each node lazily materializes its children through
//! `lookup` / `readdir`.
//!
//! The tree files are disjoint: `/dev`, `/irq`, `/mem`, `/mnt`, `/pci`,
//! `/tasks` and `/proc/self` each own their inode ranges and never refer to
//! one another's nodes.  `/tasks` and `/proc/self` are x86_64-only (they
//! walk the `proc` scheduler, which does not exist on riscv64).

pub mod dev;
pub mod irq;
pub mod mem;
pub mod mnt;
pub mod pci;
#[cfg(target_arch = "x86_64")]
pub mod proc_self;
#[cfg(target_arch = "x86_64")]
pub mod tasks;

/// Serve a read-only text payload into `buf` at `offset` (stateless, the
/// stateless-readdir discipline).  Shared by the functional op files across
/// the synthetic trees.  Returns the number of bytes written; `0` past EOF.
pub(crate) fn serve_text(text: &[u8], offset: u64, buf: &mut [u8]) -> usize {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    if start >= text.len() {
        return 0;
    }
    let n = core::cmp::min(buf.len(), text.len() - start);
    if n > 0 {
        buf[..n].copy_from_slice(&text[start..start + n]);
    }
    n
}
