use core::ffi::c_int;

// ── ctype: ASCII classification and conversion ────────────────────────
//
// Only ASCII behaviour is implemented (no locale).  All inputs are taken as
// unsigned bytes, matching the classic ctype contract.

#[inline]
fn b(c: c_int) -> u8 {
    (c as u32 & 0xFF) as u8
}

#[unsafe(no_mangle)]
pub extern "C" fn isalpha(c: c_int) -> c_int {
    (b(c).is_ascii_alphabetic()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isdigit(c: c_int) -> c_int {
    (b(c).is_ascii_digit()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isalnum(c: c_int) -> c_int {
    (b(c).is_ascii_alphanumeric()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isupper(c: c_int) -> c_int {
    (b(c).is_ascii_uppercase()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn islower(c: c_int) -> c_int {
    (b(c).is_ascii_lowercase()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isspace(c: c_int) -> c_int {
    (matches!(b(c), b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn ispunct(c: c_int) -> c_int {
    let v = b(c);
    (v.is_ascii_punctuation()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isxdigit(c: c_int) -> c_int {
    (b(c).is_ascii_hexdigit()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn iscntrl(c: c_int) -> c_int {
    let v = b(c);
    ((v < 0x20) || v == 0x7F) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isprint(c: c_int) -> c_int {
    let v = b(c);
    (v >= 0x20 && v < 0x7F) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isgraph(c: c_int) -> c_int {
    let v = b(c);
    (v > 0x20 && v < 0x7F) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isblank(c: c_int) -> c_int {
    (matches!(b(c), b' ' | b'\t')) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn tolower(c: c_int) -> c_int {
    b(c).to_ascii_lowercase() as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn toupper(c: c_int) -> c_int {
    b(c).to_ascii_uppercase() as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isascii(c: c_int) -> c_int {
    ((c as u32) < 0x80) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn toascii(c: c_int) -> c_int {
    (c & 0x7F) as c_int
}