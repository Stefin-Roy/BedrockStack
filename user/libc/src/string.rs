use core::ffi::{c_char, c_int, c_long, c_void};

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
pub extern "C" fn strtol(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long {
    unsafe {
        let mut i = 0usize;
        while *s.add(i) as u8 == b' ' || *s.add(i) as u8 == b'\t' {
            i += 1;
        }
        let mut sign = 1i64;
        if *s.add(i) as u8 == b'-' {
            sign = -1;
            i += 1;
        } else if *s.add(i) as u8 == b'+' {
            i += 1;
        }
        let base = base as u64;
        let mut out = 0i64;
        loop {
            let b = *s.add(i) as u8;
            let d = match b {
                b'0'..=b'9' => (b - b'0') as u64,
                b'a'..=b'z' => (b - b'a' + 10) as u64,
                b'A'..=b'Z' => (b - b'A' + 10) as u64,
                _ => break,
            };
            if base < 2 || d >= base {
                break;
            }
            out = out.wrapping_mul(base as i64).wrapping_add(d as i64);
            i += 1;
        }
        if !endptr.is_null() {
            *endptr = s.add(i) as *mut c_char;
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
