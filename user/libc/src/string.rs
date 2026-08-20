use core::ffi::{c_char, c_int, c_void};

#[unsafe(no_mangle)]
pub extern "C" fn strlen(s: *const c_char) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    unsafe {
        while *s.add(n) != 0 {
            n += 1;
        }
    }
    n
}

#[unsafe(no_mangle)]
pub extern "C" fn strnlen(s: *const c_char, maxlen: usize) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    unsafe {
        while n < maxlen && *s.add(n) != 0 {
            n += 1;
        }
    }
    n
}

#[unsafe(no_mangle)]
pub extern "C" fn strcmp(a: *const c_char, b: *const c_char) -> c_int {
    unsafe {
        let mut i = 0usize;
        loop {
            let ca = *a.add(i) as u8;
            let cb = *b.add(i) as u8;
            if ca != cb {
                return (ca as c_int) - (cb as c_int);
            }
            if ca == 0 {
                return 0;
            }
            i += 1;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn strncmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
    unsafe {
        for i in 0..n {
            let ca = *a.add(i) as u8;
            let cb = *b.add(i) as u8;
            if ca != cb {
                return (ca as c_int) - (cb as c_int);
            }
            if ca == 0 {
                return 0;
            }
        }
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn strcpy(dst: *mut c_char, src: *const c_char) -> *mut c_char {
    unsafe {
        let mut i = 0usize;
        loop {
            let c = *src.add(i);
            *dst.add(i) = c;
            if c == 0 {
                break;
            }
            i += 1;
        }
    }
    dst
}

#[unsafe(no_mangle)]
pub extern "C" fn strncpy(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
    unsafe {
        let mut i = 0usize;
        while i < n && *src.add(i) != 0 {
            *dst.add(i) = *src.add(i);
            i += 1;
        }
        while i < n {
            *dst.add(i) = 0;
            i += 1;
        }
    }
    dst
}

#[unsafe(no_mangle)]
pub extern "C" fn strcat(dst: *mut c_char, src: *const c_char) -> *mut c_char {
    unsafe {
        let mut i = 0usize;
        while *dst.add(i) != 0 {
            i += 1;
        }
        let mut j = 0usize;
        loop {
            let c = *src.add(j);
            *dst.add(i + j) = c;
            if c == 0 {
                break;
            }
            j += 1;
        }
    }
    dst
}

#[unsafe(no_mangle)]
pub extern "C" fn strncat(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
    unsafe {
        let mut i = 0usize;
        while *dst.add(i) != 0 {
            i += 1;
        }
        let mut j = 0usize;
        while j < n && *src.add(j) != 0 {
            *dst.add(i + j) = *src.add(j);
            j += 1;
        }
        *dst.add(i + j) = 0;
    }
    dst
}

#[unsafe(no_mangle)]
pub extern "C" fn strchr(s: *const c_char, c: c_int) -> *mut c_char {
    let target = (c & 0xFF) as u8;
    unsafe {
        let mut i = 0usize;
        loop {
            let b = *s.add(i) as u8;
            if b == target {
                return s.add(i) as *mut c_char;
            }
            if b == 0 {
                return core::ptr::null_mut();
            }
            i += 1;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn strrchr(s: *const c_char, c: c_int) -> *mut c_char {
    let target = (c & 0xFF) as u8;
    unsafe {
        let len = strlen(s);
        let mut i = len;
        loop {
            if (*s.add(i) as u8) == target {
                return s.add(i) as *mut c_char;
            }
            if i == 0 {
                return core::ptr::null_mut();
            }
            i -= 1;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn strstr(haystack: *const c_char, needle: *const c_char) -> *mut c_char {
    if needle.is_null() {
        return haystack as *mut c_char;
    }
    unsafe {
        let nlen = strlen(needle);
        if nlen == 0 {
            return haystack as *mut c_char;
        }
        let hlen = strlen(haystack);
        if nlen > hlen {
            return core::ptr::null_mut();
        }
        for i in 0..=(hlen - nlen) {
            let mut j = 0usize;
            while j < nlen && *haystack.add(i + j) == *needle.add(j) {
                j += 1;
            }
            if j == nlen {
                return haystack.add(i) as *mut c_char;
            }
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn memchr(s: *const c_void, c: c_int, n: usize) -> *mut c_void {
    let target = (c & 0xFF) as u8;
    unsafe {
        let p = s as *const u8;
        for i in 0..n {
            if *p.add(i) == target {
                return p.add(i) as *mut c_void;
            }
        }
    }
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn atoi(s: *const c_char) -> c_int {
    let mut out = 0i32;
    let mut i = 0usize;
    unsafe {
        while *s.add(i) as u8 == b' ' || *s.add(i) as u8 == b'\t' {
            i += 1;
        }
        let mut sign = 1i32;
        if *s.add(i) as u8 == b'-' {
            sign = -1;
            i += 1;
        } else if *s.add(i) as u8 == b'+' {
            i += 1;
        }
        loop {
            let b = *s.add(i) as u8;
            if !b.is_ascii_digit() {
                break;
            }
            out = out.wrapping_mul(10).wrapping_add((b - b'0') as i32);
            i += 1;
        }
        out * sign
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn strdup(s: *const c_char) -> *mut c_char {
    if s.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let n = strlen(s);
        let p = crate::mem::malloc(n + 1);
        if p.is_null() {
            return core::ptr::null_mut();
        }
        core::ptr::copy_nonoverlapping(s as *const u8, p as *mut u8, n + 1);
        p as *mut c_char
    }
}

/// Length of the initial segment of `s` made of bytes in `accept`.
#[unsafe(no_mangle)]
pub extern "C" fn strspn(s: *const c_char, accept: *const c_char) -> usize {
    if s.is_null() || accept.is_null() {
        return 0;
    }
    unsafe {
        let mut set = [false; 256];
        let mut i = 0usize;
        while *accept.add(i) != 0 {
            set[*accept.add(i) as u8 as usize] = true;
            i += 1;
        }
        let mut n = 0usize;
        while *s.add(n) != 0 && set[*s.add(n) as u8 as usize] {
            n += 1;
        }
        n
    }
}

/// Length of the initial segment of `s` made of bytes NOT in `reject`.
#[unsafe(no_mangle)]
pub extern "C" fn strcspn(s: *const c_char, reject: *const c_char) -> usize {
    if s.is_null() || reject.is_null() {
        return 0;
    }
    unsafe {
        let mut set = [false; 256];
        let mut i = 0usize;
        while *reject.add(i) != 0 {
            set[*reject.add(i) as u8 as usize] = true;
            i += 1;
        }
        let mut n = 0usize;
        while *s.add(n) != 0 && !set[*s.add(n) as u8 as usize] {
            n += 1;
        }
        n
    }
}

/// Locate the first byte of `s` that is in `accept`.
#[unsafe(no_mangle)]
pub extern "C" fn strpbrk(s: *const c_char, accept: *const c_char) -> *mut c_char {
    if s.is_null() || accept.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let mut set = [false; 256];
        let mut i = 0usize;
        while *accept.add(i) != 0 {
            set[*accept.add(i) as u8 as usize] = true;
            i += 1;
        }
        let mut n = 0usize;
        while *s.add(n) != 0 {
            if set[*s.add(n) as u8 as usize] {
                return s.add(n) as *mut c_char;
            }
            n += 1;
        }
        core::ptr::null_mut()
    }
}

/// Case-insensitive comparison (ASCII only), like glibc's `strcasecmp`.
#[unsafe(no_mangle)]
pub extern "C" fn strcasecmp(a: *const c_char, b: *const c_char) -> c_int {
    unsafe {
        let mut i = 0usize;
        loop {
            let ca = (*a.add(i) as u8).to_ascii_lowercase();
            let cb = (*b.add(i) as u8).to_ascii_lowercase();
            if ca != cb {
                return (ca as c_int) - (cb as c_int);
            }
            if ca == 0 {
                return 0;
            }
            i += 1;
        }
    }
}

/// Case-insensitive comparison, at most `n` bytes (ASCII only).
#[unsafe(no_mangle)]
pub extern "C" fn strncasecmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
    unsafe {
        for i in 0..n {
            let ca = (*a.add(i) as u8).to_ascii_lowercase();
            let cb = (*b.add(i) as u8).to_ascii_lowercase();
            if ca != cb {
                return (ca as c_int) - (cb as c_int);
            }
            if ca == 0 {
                return 0;
            }
        }
        0
    }
}

/// POSIX `strcoll` — bytewise collation order; equivalent to `strcmp`.
#[unsafe(no_mangle)]
pub extern "C" fn strcoll(a: *const c_char, b: *const c_char) -> c_int {
    strcmp(a, b)
}

/// POSIX `strxfrm` — copies `n` bytes (no locale transform); returns the
/// (untransformed) source length.
#[unsafe(no_mangle)]
pub extern "C" fn strxfrm(dst: *mut c_char, src: *const c_char, n: usize) -> usize {
    let len = strnlen(src, usize::MAX);
    if n == 0 {
        return len;
    }
    unsafe {
        let m = core::cmp::min(len, n - 1);
        core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, m);
        *dst.add(m) = 0;
    }
    len
}

/// Reentrant tokeniser. Splits `s` (mutated in place) on the bytes in `delim`.
/// With a null `s`, resumes from the static pointer saved in `saveptr`.
#[unsafe(no_mangle)]
pub extern "C" fn strtok_r(
    s: *mut c_char,
    delim: *const c_char,
    saveptr: *mut *mut c_char,
) -> *mut c_char {
    // `s == NULL` is the resume-from-saveptr continuation, NOT an error; only
    // a missing delimiter set or save pointer is. Returning early on a null
    // `s` would make every second token NULL.
    if delim.is_null() || saveptr.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let mut set = [false; 256];
        let mut i = 0usize;
        while *delim.add(i) != 0 {
            set[*delim.add(i) as u8 as usize] = true;
            i += 1;
        }
        let mut p = if s.is_null() { *saveptr } else { s };
        if p.is_null() {
            return core::ptr::null_mut();
        }
        // Skip leading delimiters.
        while *p != 0 && set[*p as u8 as usize] {
            p = p.add(1);
        }
        if *p == 0 {
            *saveptr = p;
            return core::ptr::null_mut();
        }
        let tok = p;
        while *p != 0 && !set[*p as u8 as usize] {
            p = p.add(1);
        }
        if *p != 0 {
            *p = 0;
            *saveptr = p.add(1);
        } else {
            *saveptr = p;
        }
        tok
    }
}

/// Non-reentrant tokeniser over a static save pointer.
#[unsafe(no_mangle)]
pub extern "C" fn strtok(s: *mut c_char, delim: *const c_char) -> *mut c_char {
    static mut SAVE: *mut c_char = core::ptr::null_mut();
    unsafe { strtok_r(s, delim, core::ptr::addr_of_mut!(SAVE)) }
}
