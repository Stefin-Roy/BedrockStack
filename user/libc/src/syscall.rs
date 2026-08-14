pub const SYS_READ: usize = 0;
pub const SYS_WRITE: usize = 1;

/// Invoke a kernel syscall. rdi/rsi/rdx = primary args, r10 = the optional
/// provider-defined `arg4`/flags word. `syscall` clobbers both `rcx` and
/// `r11`, so they are declared as dummy outputs. The result is signed:
/// errors come back as negative errnos.
#[inline]
pub unsafe fn syscall(n: usize, a: usize, b: usize, c: usize, d: usize) -> isize {
    let ret: isize;
    let _rcx: u64;
    let _r11: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            out("rcx") _rcx,
            out("r11") _r11,
            options(nostack),
        );
    }
    ret
}

/// Raw read. `path` must be NUL-terminated. Returns bytes read or -errno.
pub unsafe fn read_path(path: &[u8], buf: &mut [u8], flags: u64) -> isize {
    syscall(SYS_READ, path.as_ptr() as usize, buf.as_mut_ptr() as usize, buf.len(), flags as usize)
}

/// Raw write. `path` NUL-terminated. `buf[..len]` is the input; on return the
/// provider output (if any) is in `buf[..ret]`. Returns output bytes or -errno.
/// The buffer is consumed in place and zero-filled past the output.
pub unsafe fn write_path(path: &[u8], buf: &mut [u8], len: usize, flags: u64) -> isize {
    syscall(SYS_WRITE, path.as_ptr() as usize, buf.as_mut_ptr() as usize, len, flags as usize)
}

/// Chunked write that does NOT clobber the caller's `data`: copies each
/// 256-byte chunk into a stack scratch, writes it, returns total bytes on
/// success or -errno.
pub fn write_data(path: &[u8], data: &[u8], flags: u64) -> isize {
    let mut off = 0usize;
    while off < data.len() {
        let n = core::cmp::min(data.len() - off, 256);
        let mut scratch = [0u8; 256];
        scratch[..n].copy_from_slice(&data[off..off + n]);
        let r = unsafe { write_path(path, &mut scratch, n, flags) };
        if r < 0 {
            return r;
        }
        off += n;
    }
    data.len() as isize
}
