//! Tiny x86-64 disassembler for fault dumps.
//!
//! Decodes common instructions into Intel-syntax assembly.
//! Outputs directly to a `core::fmt::Write` -- no allocation.
//! Unknown opcodes fall back to printing the raw bytes.

use core::fmt::Write;

// ---------------------------------------------------------------------------
// Register-name tables
// ---------------------------------------------------------------------------
const REG64: [&str; 16] = [
    "rax","rcx","rdx","rbx","rsp","rbp","rsi","rdi",
    "r8","r9","r10","r11","r12","r13","r14","r15",
];
const REG32: [&str; 16] = [
    "eax","ecx","edx","ebx","esp","ebp","esi","edi",
    "r8d","r9d","r10d","r11d","r12d","r13d","r14d","r15d",
];
const REG16: [&str; 16] = [
    "ax","cx","dx","bx","sp","bp","si","di",
    "r8w","r9w","r10w","r11w","r12w","r13w","r14w","r15w",
];
const REG8L: [&str; 16] = [
    "al","cl","dl","bl","spl","bpl","sil","dil",
    "r8b","r9b","r10b","r11b","r12b","r13b","r14b","r15b",
];
const REG8H: [&str; 8] = [
    "al","cl","dl","bl","ah","ch","dh","bh",
];

// ---------------------------------------------------------------------------
// Mnemonic tables
// ---------------------------------------------------------------------------
const GRP1: [&str; 8] = ["add","or","adc","sbb","and","sub","xor","cmp"];
const GRP2: [&str; 8] = ["rol","ror","rcl","rcr","shl","shr","?","sar"];
const GRP3: [&str; 8] = ["test","test","not","neg","mul","imul","div","idiv"];
const JCC: [&str; 16] = [
    "jo","jno","jb","jnb","jz","jnz","jbe","ja",
    "js","jns","jp","jnp","jl","jnl","jle","jg",
];
const CMOVCC: [&str; 16] = [
    "cmovo","cmovno","cmovb","cmovae","cmove","cmovne","cmovbe","cmova",
    "cmovs","cmovns","cmovp","cmovnp","cmovl","cmovge","cmovle","cmovg",
];
const SETCC: [&str; 16] = [
    "seto","setno","setb","setae","sete","setne","setbe","seta",
    "sets","setns","setp","setnp","setl","setge","setle","setg",
];
const SYSOP: [&str; 8] = [
    "sgdt","sidt","lgdt","lidt","smsw","?","lmsw","?",
];

fn reg64(rm: u8, ext: u8) -> &'static str { REG64[(rm | (ext << 3)) as usize] }
fn reg32(rm: u8, ext: u8) -> &'static str { REG32[(rm | (ext << 3)) as usize] }
fn reg16(rm: u8, ext: u8) -> &'static str { REG16[(rm | (ext << 3)) as usize] }
fn reg8(rm: u8, rex: bool, ext: u8) -> &'static str {
    if rex { REG8L[(rm | (ext << 3)) as usize] } else { REG8H[(rm | (ext << 3)) as usize] }
}

fn write_reg(w: &mut impl Write, idx: u8, byte: bool, wide: bool, rex: bool, ext: u8, opsz16: bool) {
    if byte {
        let _ = write!(w, "{}", reg8(idx, rex, ext));
    } else if opsz16 {
        let _ = write!(w, "{}", reg16(idx, ext));
    } else if wide {
        let _ = write!(w, "{}", reg64(idx, ext));
    } else {
        let _ = write!(w, "{}", reg32(idx, ext));
    }
}

// ---------------------------------------------------------------------------
// Effective-address formatter  (Intel syntax)
// ---------------------------------------------------------------------------
/// Write a ModRM-based memory operand. `bytes[0]` must be the ModRM byte.
/// Returns `Some(total_bytes_consumed)` including the ModRM byte,
/// or `None` if the buffer is too short.
fn write_ea(w: &mut impl Write, bytes: &[u8], addr32: bool, ext_b: u8, ext_x: u8, byte_sz: bool, wide: bool, has_rex: bool, opsz16: bool) -> Option<usize> {
    if bytes.is_empty() { return None; }
    let modrm = bytes[0];
    let md = modrm >> 6;
    let rm = modrm & 7;

    if md == 3 {
        write_reg(w, rm, byte_sz, wide, has_rex, ext_b, opsz16);
        return Some(1);
    }

    // RIP-relative (64-bit addressing only)
    if !addr32 && md == 0 && rm == 5 {
        let extra = &bytes[1..];
        if extra.len() < 4 { return None; }
        let d = i32::from_le_bytes(extra[..4].try_into().unwrap());
        let _ = write!(w, "[rip{:+}]", d);
        return Some(5);
    }

    // 32-bit absolute addressing (with 0x67 prefix)
    if addr32 && md == 0 && rm == 5 {
        let extra = &bytes[1..];
        if extra.len() < 4 { return None; }
        let d = u32::from_le_bytes(extra[..4].try_into().unwrap());
        let _ = write!(w, "[{:#x}]", d);
        return Some(5);
    }

    // ---- No-SIB form ----
    if rm != 4 {
        let (disp, sz): (i64, usize) = match md {
            0 => (0, 0),
            1 => {
                let extra = &bytes[1..];
                if extra.is_empty() { return None; }
                (extra[0] as i8 as i64, 1)
            }
            _ => {
                let extra = &bytes[1..];
                if extra.len() < 4 { return None; }
                (i32::from_le_bytes(extra[..4].try_into().unwrap()) as i64, 4)
            }
        };
        let base = if addr32 { reg32(rm, ext_b) } else { reg64(rm, ext_b) };
        if md == 0 {
            let _ = write!(w, "[{}]", base);
        } else {
            let _ = write!(w, "[{}{:+}]", base, disp);
        }
        return Some(1 + sz);
    }

    // ---- SIB form ----
    let extra = &bytes[1..];
    if extra.is_empty() { return None; }
    let sib = extra[0];
    let ss = sib >> 6;
    let sidx = (sib >> 3) & 7;
    let sbase = sib & 7;
    let scale = 1u64 << ss;
    let mut consumed = 2; // ModRM + SIB

    let has_base = !(md == 0 && sbase == 5);
    let disp: i64;
    if md == 0 && sbase == 5 {
        if extra.len() < 5 { return None; }
        disp = i32::from_le_bytes(extra[1..5].try_into().unwrap()) as i64;
        consumed += 4;
    } else if md == 1 {
        if extra.len() < 2 { return None; }
        disp = extra[1] as i8 as i64;
        consumed += 1;
    } else if md == 2 {
        if extra.len() < 5 { return None; }
        disp = i32::from_le_bytes(extra[1..5].try_into().unwrap()) as i64;
        consumed += 4;
    } else {
        disp = 0;
    }
    let have_idx = sidx != 4;

    let _ = write!(w, "[");
    if has_base {
        let r = if addr32 { reg32(sbase, ext_b) } else { reg64(sbase, ext_b) };
        let _ = write!(w, "{}", r);
    }
    if have_idx {
        let r = if addr32 { reg32(sidx, ext_x) } else { reg64(sidx, ext_x) };
        let _ = write!(w, "+{}*{}", r, scale);
    }
    if disp != 0 || (!has_base && !have_idx) {
        let _ = write!(w, "{:+}", disp);
    }
    let _ = write!(w, "]");
    Some(consumed)
}

// ---------------------------------------------------------------------------
// ModRM two-operand helper  (dest ← src  order for Intel syntax)
// ---------------------------------------------------------------------------
fn modrm_rm(
    w: &mut impl Write,
    bytes: &[u8],
    to_reg: bool,
    byte_sz: bool,
    wide: bool,
    rex: bool,
    ext_r: u8,
    ext_b: u8,
    ext_x: u8,
    addr32: bool,
    opsz16: bool,
) -> Option<usize> {
    if bytes.is_empty() { return None; }
    let modrm = bytes[0];
    let reg = (modrm >> 3) & 7;

    if to_reg {
        // reg (dest)
        write_reg(w, reg, byte_sz, wide, rex, ext_r, opsz16);
        let _ = write!(w, ",");
        // r/m (src)
        write_ea(w, bytes, addr32, ext_b, ext_x, byte_sz, wide, rex, opsz16)
    } else {
        // r/m (dest)
        let c = write_ea(w, bytes, addr32, ext_b, ext_x, byte_sz, wide, rex, opsz16)?;
        let _ = write!(w, ",");
        write_reg(w, reg, byte_sz, wide, rex, ext_r, opsz16);
        Some(c)
    }
}

// ---------------------------------------------------------------------------
// Main disassembler entry point
// ---------------------------------------------------------------------------
/// Decode a single x86-64 instruction.
///
/// Writes the mnemonic and Intel-syntax operands to `w`.
/// Returns the instruction length in bytes (0 if `bytes` is empty).
pub fn disasm_one(addr: u64, bytes: &[u8], w: &mut impl Write) -> Option<usize> {
    if bytes.is_empty() { return None; }
    let mut pos = 0usize;
    let mut rex = 0u8;
    let mut opsz16 = false;
    let mut addrsz32 = false;

    // Parse prefixes
    loop {
        if pos >= bytes.len() { return None; }
        match bytes[pos] {
            0x40..=0x4F => rex = bytes[pos],
            0x66 => opsz16 = true,
            0x67 => addrsz32 = true,
            0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 => {}
            0xF0 | 0xF2 | 0xF3 => {}
            _ => break,
        }
        pos += 1;
    }
    if pos >= bytes.len() { return None; }

    let ext_r = (rex >> 2) & 1;
    let ext_x = (rex >> 1) & 1;
    let ext_b = rex & 1;
    let rex_w = (rex >> 3) & 1;
    let has_rex = rex != 0;
    let wide = rex_w != 0;
    let byte_sz = false;

    let op = bytes[pos];
    pos += 1;
    let rest = &bytes[pos..];

    match op {
        // -- ALU (ModRM forms): 0x00-0x3B step 8, 4 each --
        b @ 0x00..=0x03 | b @ 0x08..=0x0B | b @ 0x10..=0x13
        | b @ 0x18..=0x1B | b @ 0x20..=0x23 | b @ 0x28..=0x2B
        | b @ 0x30..=0x33 | b @ 0x38..=0x3B => {
            let grp = ((b >> 3) & 7) as usize;
            let to_reg = (b & 2) != 0;
            let byte_sz = (b & 1) == 0;
            let _ = write!(w, "{} ", GRP1[grp]);
            if let Some(c) = modrm_rm(w, rest, to_reg, byte_sz, wide, has_rex, ext_r, ext_b, ext_x, addrsz32, opsz16) {
                pos += c;
            }
        }

        // -- ALU AL/EAX, imm8 --
        b @ 0x04 | b @ 0x0C | b @ 0x14 | b @ 0x1C
        | b @ 0x24 | b @ 0x2C | b @ 0x34 | b @ 0x3C => {
            let grp = ((b >> 3) & 7) as usize;
            let _ = write!(w, "{} al,", GRP1[grp]);
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "{:#x}", v); }
        }

        // -- ALU EAX/RAX, imm32 --
        b @ 0x05 | b @ 0x0D | b @ 0x15 | b @ 0x1D
        | b @ 0x25 | b @ 0x2D | b @ 0x35 | b @ 0x3D => {
            let grp = ((b >> 3) & 7) as usize;
            if wide { let _ = write!(w, "{} rax,", GRP1[grp]); }
            else { let _ = write!(w, "{} eax,", GRP1[grp]); }
            if pos + 4 <= bytes.len() {
                let v = u32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                pos += 4;
                let _ = write!(w, "{:#x}", v);
            }
        }

        // -- invalid in 64-bit mode --
        0x06 | 0x07 | 0x0E | 0x16 | 0x17 | 0x1E | 0x1F
        | 0x27 | 0x2F | 0x37 | 0x3F | 0x60 | 0x61
        | 0x62 | 0x9A | 0xC4 | 0xC5 | 0xCE | 0xD4
        | 0xD5 | 0xD6 | 0xEA => {
            let _ = write!(w, "db {:#x}", op);
        }

        // -- PUSH reg / POP reg --
        0x50..=0x57 => { let _ = write!(w, "push {}", reg64(op - 0x50, ext_b)); }
        0x58..=0x5F => { let _ = write!(w, "pop {}", reg64(op - 0x58, ext_b)); }

        // -- MOVSXD --
        0x63 => {
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let reg = (modrm >> 3) & 7;
            let _ = write!(w, "movsxd {},", reg64(reg, ext_r));
            if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                pos += c;
            }
        }

        // -- PUSH imm32 / imm8, IMUL --
        0x68 => {
            if pos + 4 <= bytes.len() {
                let v = i32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                if v >= 0 { let _ = write!(w, "push {:#x}", v); }
                else { let _ = write!(w, "push -{:#x}", -v); }
                pos += 4;
            }
        }
        0x6A => {
            if pos < bytes.len() { let v = bytes[pos] as i8 as i64; pos += 1;
                if v >= 0 { let _ = write!(w, "push {:#x}", v); }
                else { let _ = write!(w, "push -{:#x}", -v); }
            }
        }
        0x69 | 0x6B => {
            let imm_sz = if op == 0x69 { 4usize } else { 1 };
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let reg = (modrm >> 3) & 7;
            let _ = write!(w, "imul {},", reg64(reg, ext_r));
            let c1 = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c1;
            let _ = write!(w, ",");
            if imm_sz == 4 && pos + 4 <= bytes.len() {
                let v = i32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                if v >= 0 { let _ = write!(w, "{:#x}", v); pos += 4; }
                else { let _ = write!(w, "-{:#x}", -v); pos += 4; }
            } else if imm_sz == 1 && pos < bytes.len() {
                let v = bytes[pos] as i8 as i64; pos += 1;
                if v >= 0 { let _ = write!(w, "{:#x}", v); }
                else { let _ = write!(w, "-{:#x}", -v); }
            }
        }

        // -- Jcc rel8 --
        0x70..=0x7F => {
            let cc = (op & 0xF) as usize;
            if pos < bytes.len() {
                let off = bytes[pos] as i8 as i64;
                pos += 1;
                let target = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                let _ = write!(w, "{} {:#x}", JCC[cc], target);
            }
        }

        // -- Group 1 immediate (0x80-0x83) --
        0x80 | 0x81 | 0x82 | 0x83 => {
            let byte_sz = op == 0x80 || op == 0x82;
            let byte_imm = op == 0x80 || op == 0x82;
            let wide_imm = op == 0x81;
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let reg_idx = (modrm >> 3) & 7;
            if reg_idx == 0 && (modrm >> 6) == 3 {
                // TEST r/m,r/m (special case: Grp1 /0 with mod=11 is TEST)
                // Actually in group 1, /0 is ADD. TEST is Group 3.
                // This case doesn't apply.
            }
            let _ = write!(w, "{} ", GRP1[reg_idx as usize]);
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",");
            if byte_imm {
                if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "{:#x}", v); }
            } else if wide_imm {
                if pos + 4 <= bytes.len() {
                    let v = i32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                    pos += 4;
                    let _ = write!(w, "{:#x}", v);
                }
            } else {
                // 0x83: sign-extended imm8
                if pos < bytes.len() { let v = bytes[pos] as i8 as i64; pos += 1;
                    if v >= 0 { let _ = write!(w, "{:#x}", v); }
                    else { let _ = write!(w, "-{:#x}", -v); }
                }
            }
        }

        // -- TEST r/m, r --
        0x84 | 0x85 => {
            let byte_sz = op == 0x84;
            let _ = write!(w, "test ");
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let reg = (modrm >> 3) & 7;
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",");
            write_reg(w, reg, byte_sz, wide, has_rex, ext_r, opsz16);
        }

        // -- XCHG r/m, r --
        0x86 | 0x87 => {
            let byte_sz = op == 0x86;
            let _ = write!(w, "xchg ");
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let reg = (modrm >> 3) & 7;
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",");
            write_reg(w, reg, byte_sz, wide, has_rex, ext_r, opsz16);
        }

        // -- MOV r/m, r  /  MOV r, r/m --
        0x88 | 0x89 | 0x8A | 0x8B => {
            let to_reg = op == 0x8A || op == 0x8B;
            let byte_sz = op == 0x88 || op == 0x8A;
            let _ = write!(w, "mov ");
            if let Some(c) = modrm_rm(w, rest, to_reg, byte_sz, wide, has_rex, ext_r, ext_b, ext_x, addrsz32, opsz16) {
                pos += c;
            }
        }

        // -- MOV r/m, sreg --
        0x8C => {
            let _ = write!(w, "mov ");
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let sreg = (modrm >> 3) & 7;
            let sreg_name = match sreg { 0 => "es", 1 => "cs", 2 => "ss", 3 => "ds", 4 => "fs", 5 => "gs", _ => "?" };
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",{}", sreg_name);
        }

        // -- LEA --
        0x8D => {
            let _ = write!(w, "lea ");
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let reg = (modrm >> 3) & 7;
            let _ = write!(w, "{}", reg64(reg, ext_r));
            let _ = write!(w, ",");
            if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                pos += c;
            }
        }

        // -- MOV sreg, r/m --
        0x8E => {
            let _ = write!(w, "mov ");
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let sreg = (modrm >> 3) & 7;
            let sreg_name = match sreg { 0 => "es", 1 => "cs", 2 => "ss", 3 => "ds", 4 => "fs", 5 => "gs", _ => "?" };
            let _ = write!(w, "{},", sreg_name);
            if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                pos += c;
            }
        }

        // -- POP r/m --
        0x8F => {
            let _ = write!(w, "pop ");
            if rest.is_empty() { return Some(pos); }
            if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                pos += c;
            }
        }

        // -- NOP --
        0x90 => {
            if has_rex {
                // With REX prefix, 0x90 is still NOP (REX doesn't change it)
            }
            let _ = write!(w, "nop");
        }

        // -- XCHG reg, EAX/RAX --
        0x91..=0x97 => {
            let r = op - 0x91;
            if wide { let _ = write!(w, "xchg rax,{}", reg64(r, ext_b)); }
            else { let _ = write!(w, "xchg eax,{}", reg32(r, ext_b)); }
        }

        // -- CWDE / CDQE / CWD / CDQ / CQO --
        0x98 => { if wide { let _ = write!(w, "cdqe"); } else { let _ = write!(w, "cwde"); } }
        0x99 => { if wide { let _ = write!(w, "cqo"); } else { let _ = write!(w, "cdq"); } }

        // -- FWAIT, PUSHF/POPF, SAHF, LAHF --
        0x9B => { let _ = write!(w, "fwait"); }
        0x9C => { let _ = write!(w, "pushfq"); }
        0x9D => { let _ = write!(w, "popfq"); }
        0x9E => { let _ = write!(w, "sahf"); }
        0x9F => { let _ = write!(w, "lahf"); }

        // -- MOV AL/EAX/RAX, moffs --
        0xA0 => {
            if pos + 8 <= bytes.len() {
                let v = u64::from_le_bytes(bytes[pos..pos+8].try_into().unwrap());
                pos += 8;
                let _ = write!(w, "mov al,[{:#x}]", v);
            }
        }
        0xA1 => {
            if pos + 8 <= bytes.len() {
                let v = u64::from_le_bytes(bytes[pos..pos+8].try_into().unwrap());
                pos += 8;
                if wide { let _ = write!(w, "mov rax,[{:#x}]", v); }
                else { let _ = write!(w, "mov eax,[{:#x}]", v); }
            }
        }
        0xA2 => {
            if pos + 8 <= bytes.len() {
                let v = u64::from_le_bytes(bytes[pos..pos+8].try_into().unwrap());
                pos += 8;
                let _ = write!(w, "mov [{:#x}],al", v);
            }
        }
        0xA3 => {
            if pos + 8 <= bytes.len() {
                let v = u64::from_le_bytes(bytes[pos..pos+8].try_into().unwrap());
                pos += 8;
                if wide { let _ = write!(w, "mov [{:#x}],rax", v); }
                else { let _ = write!(w, "mov [{:#x}],eax", v); }
            }
        }

        // -- MOVS, CMPS, STOS, LODS, SCAS --
        0xA4 => { let _ = write!(w, "movsb"); }
        0xA5 => { if wide { let _ = write!(w, "movsq"); } else { let _ = write!(w, "movsd"); } }
        0xA6 => { let _ = write!(w, "cmpsb"); }
        0xA7 => { if wide { let _ = write!(w, "cmpsq"); } else { let _ = write!(w, "cmpsd"); } }
        0xAA => { let _ = write!(w, "stosb"); }
        0xAB => { if wide { let _ = write!(w, "stosq"); } else { let _ = write!(w, "stosd"); } }
        0xAC => { let _ = write!(w, "lodsb"); }
        0xAD => { if wide { let _ = write!(w, "lodsq"); } else { let _ = write!(w, "lodsd"); } }
        0xAE => { let _ = write!(w, "scasb"); }
        0xAF => { if wide { let _ = write!(w, "scasq"); } else { let _ = write!(w, "scasd"); } }

        // -- TEST AL/EAX, imm --
        0xA8 => {
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "test al,{:#x}", v); }
        }
        0xA9 => {
            if pos + 4 <= bytes.len() {
                let v = u32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                pos += 4;
                if wide { let _ = write!(w, "test rax,{:#x}", v); }
                else { let _ = write!(w, "test eax,{:#x}", v); }
            }
        }

        // -- MOV r8, imm8 --
        0xB0..=0xB7 => {
            let r = op - 0xB0;
            if pos < bytes.len() { let v = bytes[pos]; pos += 1;
                let _ = write!(w, "mov {},0x{:x}", reg8(r, has_rex, ext_b), v);
            }
        }

        // -- MOV r64, imm64 --
        0xB8..=0xBF => {
            let r = op - 0xB8;
            if wide {
                if pos + 8 <= bytes.len() {
                    let v = u64::from_le_bytes(bytes[pos..pos+8].try_into().unwrap());
                    pos += 8;
                    let _ = write!(w, "mov {},0x{:x}", reg64(r, ext_b), v);
                }
            } else if opsz16 {
                if pos + 2 <= bytes.len() {
                    let v = u16::from_le_bytes(bytes[pos..pos+2].try_into().unwrap());
                    pos += 2;
                    let _ = write!(w, "mov {},0x{:x}", reg16(r, ext_b), v);
                }
            } else {
                if pos + 4 <= bytes.len() {
                    let v = u32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                    pos += 4;
                    let _ = write!(w, "mov {},0x{:x}", reg32(r, ext_b), v);
                }
            }
        }

        // -- Group 2 shift/rotate r/m, imm8 --
        0xC0 | 0xC1 => {
            let byte_sz = op == 0xC0;
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let grp = (modrm >> 3) & 7;
            let _ = write!(w, "{} ", GRP2[grp as usize]);
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",");
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "{:#x}", v); }
        }

        // -- RET --
        0xC2 => {
            if pos + 2 <= bytes.len() {
                let v = u16::from_le_bytes(bytes[pos..pos+2].try_into().unwrap());
                pos += 2;
                let _ = write!(w, "ret {:#x}", v);
            }
        }
        0xC3 => { let _ = write!(w, "ret"); }
        0xCA => {
            if pos + 2 <= bytes.len() {
                let v = u16::from_le_bytes(bytes[pos..pos+2].try_into().unwrap());
                pos += 2;
                let _ = write!(w, "retf {:#x}", v);
            }
        }
        0xCB => { let _ = write!(w, "retf"); }

        // -- MOV r/m, imm --
        0xC6 => {
            let _ = write!(w, "mov byte ");
            if rest.is_empty() { return Some(pos); }
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",");
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "{:#x}", v); }
        }
        0xC7 => {
            let _ = write!(w, "mov ");
            if rest.is_empty() { return Some(pos); }
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",");
            if pos + 4 <= bytes.len() {
                let v = u32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                pos += 4;
                let _ = write!(w, "{:#x}", v);
            }
        }

        // -- ENTER, LEAVE --
        0xC8 => {
            if pos + 4 <= bytes.len() {
                let alloc = u16::from_le_bytes(bytes[pos..pos+2].try_into().unwrap());
                let nest = bytes[pos+2];
                pos += 3;
                let _ = write!(w, "enter {:#x},{}", alloc, nest);
            }
        }
        0xC9 => { let _ = write!(w, "leave"); }

        // -- INT3, INT imm8, IRETQ --
        0xCC => { let _ = write!(w, "int3"); }
        0xCD => {
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "int {:#x}", v); }
        }
        0xCF => { let _ = write!(w, "iretq"); }

        // -- Group 2 shift/rotate r/m, 1 --
        0xD0 | 0xD1 => {
            let byte_sz = op == 0xD0;
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let grp = (modrm >> 3) & 7;
            let _ = write!(w, "{} ", GRP2[grp as usize]);
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",1");
        }

        // -- Group 2 shift/rotate r/m, CL --
        0xD2 | 0xD3 => {
            let byte_sz = op == 0xD2;
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let grp = (modrm >> 3) & 7;
            let _ = write!(w, "{} ", GRP2[grp as usize]);
            let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
            pos += c;
            let _ = write!(w, ",cl");
        }

        // -- XLAT --
        0xD7 => { let _ = write!(w, "xlat"); }

        // -- LOOPNE/LOOPE/LOOP/JRCXZ --
        0xE0 => {
            if pos < bytes.len() { let off = bytes[pos] as i8 as i64; pos += 1;
                let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                let _ = write!(w, "loopne {:#x}", t);
            }
        }
        0xE1 => {
            if pos < bytes.len() { let off = bytes[pos] as i8 as i64; pos += 1;
                let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                let _ = write!(w, "loope {:#x}", t);
            }
        }
        0xE2 => {
            if pos < bytes.len() { let off = bytes[pos] as i8 as i64; pos += 1;
                let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                let _ = write!(w, "loop {:#x}", t);
            }
        }
        0xE3 => {
            if pos < bytes.len() { let off = bytes[pos] as i8 as i64; pos += 1;
                let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                if addrsz32 { let _ = write!(w, "jecxz {:#x}", t); }
                else { let _ = write!(w, "jrcxz {:#x}", t); }
            }
        }

        // -- IN/OUT --
        0xE4 => {
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "in al,{:#x}", v); }
        }
        0xE5 => {
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "in eax,{:#x}", v); }
        }
        0xE6 => {
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "out {:#x},al", v); }
        }
        0xE7 => {
            if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "out {:#x},eax", v); }
        }
        0xEC => { let _ = write!(w, "in al,dx"); }
        0xED => { let _ = write!(w, "in eax,dx"); }
        0xEE => { let _ = write!(w, "out dx,al"); }
        0xEF => { let _ = write!(w, "out dx,eax"); }

        // -- CALL rel32, JMP rel32/rel8 --
        0xE8 => {
            if pos + 4 <= bytes.len() {
                let off = i32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap()) as i64;
                pos += 4;
                let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                let _ = write!(w, "call {:#x}", t);
            }
        }
        0xE9 => {
            if pos + 4 <= bytes.len() {
                let off = i32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap()) as i64;
                pos += 4;
                let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                let _ = write!(w, "jmp {:#x}", t);
            }
        }
        0xEB => {
            if pos < bytes.len() {
                let off = bytes[pos] as i8 as i64;
                pos += 1;
                let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                let _ = write!(w, "jmp {:#x}", t);
            }
        }

        // -- HLT, CMC --
        0xF4 => { let _ = write!(w, "hlt"); }
        0xF5 => { let _ = write!(w, "cmc"); }

        // -- Group 3 unary (TEST/NOT/NEG/MUL/IMUL/DIV/IDIV) --
        0xF6 | 0xF7 => {
            let byte_sz = op == 0xF6;
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let grp = (modrm >> 3) & 7;
            if grp == 0 || grp == 1 {
                // TEST r/m, imm -- the /0 and /1 forms of Group 3 are TEST
                let _ = write!(w, "test ");
                let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
                pos += c;
                let _ = write!(w, ",");
                if byte_sz {
                    if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "{:#x}", v); }
                } else {
                    if pos + 4 <= bytes.len() {
                        let v = u32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
                        pos += 4;
                        let _ = write!(w, "{:#x}", v);
                    }
                }
            } else {
                let _ = write!(w, "{} ", GRP3[grp as usize]);
                if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                    pos += c;
                }
            }
        }

        // -- CLC, STC, CLI, STI, CLD, STD --
        0xF8 => { let _ = write!(w, "clc"); }
        0xF9 => { let _ = write!(w, "stc"); }
        0xFA => { let _ = write!(w, "cli"); }
        0xFB => { let _ = write!(w, "sti"); }
        0xFC => { let _ = write!(w, "cld"); }
        0xFD => { let _ = write!(w, "std"); }

        // -- Group 4 (INC/DEC r/m8) --
        0xFE => {
            let byte_sz = true;
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let grp = (modrm >> 3) & 7;
            if grp == 0 { let _ = write!(w, "inc "); }
            else if grp == 1 { let _ = write!(w, "dec "); }
            else { let _ = write!(w, "? "); }
            if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                pos += c;
            }
        }

        // -- Group 5 (INC/DEC/CALL/JMP/PUSH r/m) --
        0xFF => {
            if rest.is_empty() { return Some(pos); }
            let modrm = rest[0];
            let grp = (modrm >> 3) & 7;
            match grp {
                0 => { let _ = write!(w, "inc "); }
                1 => { let _ = write!(w, "dec "); }
                2 => { let _ = write!(w, "call "); }
                3 => { let _ = write!(w, "call far "); }
                4 => { let _ = write!(w, "jmp "); }
                5 => { let _ = write!(w, "jmp far "); }
                6 => { let _ = write!(w, "push "); }
                _ => { let _ = write!(w, "? "); }
            }
            if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                pos += c;
            }
        }

        // ===========================================================
        // Two-byte opcodes (0x0F prefix)
        // ===========================================================
        0x0F => {
            if pos >= bytes.len() { return Some(pos); }
            let ext = bytes[pos]; pos += 1;
            let rest = &bytes[pos..];

            match ext {
                // -- System instructions --
                0x01 => {
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    if (modrm >> 6) == 3 {
                        // Register form -- in mod=3, opcodes like sgdt/sidt/lgdt/lidt
                        // become VMX/SVM instructions. Print correct mnemonics.
                        match (reg, modrm & 7) {
                            (0, _) => { let _ = write!(w, "vmrun"); pos += 1; }
                            (1, _) => { let _ = write!(w, "vmmcall"); pos += 1; }
                            (2, _) => { let _ = write!(w, "vmload"); pos += 1; }
                            (3, _) => { let _ = write!(w, "vmsave"); pos += 1; }
                            (4, 1) => { let _ = write!(w, "smsw"); if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; } }
                            (6, 1) => { let _ = write!(w, "lmsw"); if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; } }
                            (7, 0) => { let _ = write!(w, "swapgs"); pos += 1; }
                            (7, 1) => { let _ = write!(w, "rdtscp"); pos += 1; }
                            _ => { let _ = write!(w, "sysop {:#x},{:#x}", reg, modrm & 7); pos += 1; }
                        }
                    } else {
                        let mnem = SYSOP[reg as usize];
                        let _ = write!(w, "{} ", mnem);
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                            pos += c;
                        }
                    }
                }
                0x05 => { let _ = write!(w, "syscall"); }
                0x06 => { let _ = write!(w, "clts"); }
                0x07 => { let _ = write!(w, "sysret"); }
                0x08 => { let _ = write!(w, "invd"); }
                0x09 => { let _ = write!(w, "wbinvd"); }
                0x0B => { let _ = write!(w, "ud2"); }
                0x0D => { let _ = write!(w, "prefetch"); if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; } }
                0x0E => { let _ = write!(w, "femms"); }

                // -- 0x0F 0x1F: NOP (multi-byte) --
                0x1F => {
                    let _ = write!(w, "nop");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                        pos += c;
                    }
                }

                // -- MOV CR/DR --
                0x20 => {
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let cr = (modrm & 7) | (ext_r << 3);
                    let _ = write!(w, "mov {},cr{}", reg64(reg, ext_b), cr);
                    pos += 1;
                }
                0x21 => {
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let dr = (modrm & 7) | (ext_r << 3);
                    let _ = write!(w, "mov {},dr{}", reg64(reg, ext_b), dr);
                    pos += 1;
                }
                0x22 => {
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let cr = (modrm & 7) | (ext_r << 3);
                    let _ = write!(w, "mov cr{},{}", cr, reg64(reg, ext_b));
                    pos += 1;
                }
                0x23 => {
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let dr = (modrm & 7) | (ext_r << 3);
                    let _ = write!(w, "mov dr{},{}", dr, reg64(reg, ext_b));
                    pos += 1;
                }

                // -- WRMSR / RDTSC / RDMSR / RDPMC --
                0x30 => { let _ = write!(w, "wrmsr"); }
                0x31 => { let _ = write!(w, "rdtsc"); }
                0x32 => { let _ = write!(w, "rdmsr"); }
                0x33 => { let _ = write!(w, "rdpmc"); }

                // -- SYSENTER / SYSEXIT --
                0x34 => { let _ = write!(w, "sysenter"); }
                0x35 => { let _ = write!(w, "sysexit"); }

                // -- CMOVcc --
                0x40..=0x4F => {
                    let cc = (ext & 0xF) as usize;
                    let _ = write!(w, "{} ", CMOVCC[cc]);
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                        pos += c;
                    }
                }

                // -- MOVAPS / MOVAPD --
                0x28 | 0x29 => {
                    let to_reg = ext == 0x28;
                    let mnem = if wide { "movapd" } else { "movaps" };
                    let _ = write!(w, "{} ", mnem);
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    if to_reg {
                        let _ = write!(w, "xmm{}", reg);
                        let _ = write!(w, ",");
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; }
                    } else {
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; }
                        let _ = write!(w, ",xmm{}", reg);
                    }
                }

                // -- SIMD / SSE (0x50-0x5F) --
                0x50..=0x5F => {
                    let simd = match ext {
                        0x50 => "movmskpd", 0x51 => "sqrt",   0x52 => "rsqrt", 0x53 => "rcp",
                        0x54 => "andps",     0x55 => "andnps", 0x56 => "orps",  0x57 => "xorps",
                        0x58 => "add",       0x59 => "mul",    0x5A => "cvt",   0x5B => "cvt",
                        0x5C => "sub",       0x5D => "min",    0x5E => "div",   0x5F => "max",
                        _ => "??",
                    };
                    let _ = write!(w, "{} ", simd);
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "xmm{}", reg);
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                        pos += c;
                    }
                }

                // -- Jcc rel32 --
                0x80..=0x8F => {
                    let cc = (ext & 0xF) as usize;
                    if pos + 4 <= bytes.len() {
                        let off = i32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap()) as i64;
                        pos += 4;
                        let t = addr.wrapping_add(pos as u64).wrapping_add(off as u64);
                        let _ = write!(w, "{} {:#x}", JCC[cc], t);
                    }
                }

                // -- SETcc --
                0x90..=0x9F => {
                    let byte_sz = true;
                    let cc = (ext & 0xF) as usize;
                    if rest.is_empty() { return Some(pos); }
                    let _ = write!(w, "{} ", SETCC[cc]);
                    if rest.is_empty() { return Some(pos); }
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                        pos += c;
                    }
                }

                // -- PUSH/POP FS/GS --
                0xA0 => { let _ = write!(w, "push fs"); }
                0xA1 => { let _ = write!(w, "pop fs"); }
                0xA8 => { let _ = write!(w, "push gs"); }
                0xA9 => { let _ = write!(w, "pop gs"); }
                0xA2 => { let _ = write!(w, "cpuid"); }
                0xAA => { let _ = write!(w, "rsm"); }

                // -- BT / BTS --
                0xA3 => {
                    let _ = write!(w, "bt ");
                    if !rest.is_empty() {
                        let modrm = rest[0];
                        let r = (modrm >> 3) & 7;
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                            let _ = write!(w, ",");
                            write_reg(w, r, false, wide, has_rex, ext_r, opsz16);
                            pos += c;
                        }
                    }
                }
                0xAB => {
                    let _ = write!(w, "bts ");
                    if !rest.is_empty() {
                        let modrm = rest[0];
                        let r = (modrm >> 3) & 7;
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                            let _ = write!(w, ",");
                            write_reg(w, r, false, wide, has_rex, ext_r, opsz16);
                            pos += c;
                        }
                    }
                }

                // -- SHLD / SHRD --
                0xA5 => {
                    let _ = write!(w, "shld ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
                    pos += c;
                    let _ = write!(w, ",{},cl", reg64(reg, ext_r));
                }
                0xAD => {
                    let _ = write!(w, "shrd ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
                    pos += c;
                    let _ = write!(w, ",{},cl", reg64(reg, ext_r));
                }

                // -- IMUL r, r/m --
                0xAF => {
                    let _ = write!(w, "imul ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                        pos += c;
                    }
                }

                // -- CMPXCHG --
                0xB0 | 0xB1 => {
                    let byte_sz = ext == 0xB0;
                    let _ = write!(w, "cmpxchg ");
                    if !rest.is_empty() {
                        let modrm = rest[0];
                        let r = (modrm >> 3) & 7;
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                            let _ = write!(w, ",");
                            write_reg(w, r, byte_sz, wide, has_rex, ext_r, opsz16);
                            pos += c;
                        }
                    }
                }

                // -- LSS, LFS, LGS, BTR, BTC --
                0xB2 => {
                    let _ = write!(w, "lss ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; }
                }
                0xB4 => {
                    let _ = write!(w, "lfs ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; }
                }
                0xB5 => {
                    let _ = write!(w, "lgs ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; }
                }

                // -- MOVZX --
                0xB6 => {
                    let _ = write!(w, "movzx ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, true, wide, has_rex, opsz16) { pos += c; }
                }
                0xB7 => {
                    let _ = write!(w, "movzx ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, false, wide, has_rex, true) { pos += c; }
                }

                // -- Group 8 (BT/BTS/BTR/BTC with imm8) --
                0xBA => {
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let grp = (modrm >> 3) & 7;
                    let mnem = match grp {
                        4 => "bt", 5 => "bts", 6 => "btr", 7 => "btc",
                        _ => "?",
                    };
                    let _ = write!(w, "{} ", mnem);
                    let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
                    pos += c;
                    let _ = write!(w, ",");
                    if pos < bytes.len() { let v = bytes[pos]; pos += 1; let _ = write!(w, "{:#x}", v); }
                }

                // -- BSF / BSR --
                0xBC => {
                    let _ = write!(w, "bsf ");
                    if !rest.is_empty() {
                        let modrm = rest[0];
                        let r = (modrm >> 3) & 7;
                        write_reg(w, r, false, wide, has_rex, ext_r, opsz16);
                        let _ = write!(w, ",");
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                            pos += c;
                        }
                    }
                }
                0xBD => {
                    let _ = write!(w, "bsr ");
                    if !rest.is_empty() {
                        let modrm = rest[0];
                        let r = (modrm >> 3) & 7;
                        write_reg(w, r, false, wide, has_rex, ext_r, opsz16);
                        let _ = write!(w, ",");
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                            pos += c;
                        }
                    }
                }

                // -- MOVSX --
                0xBE => {
                    let _ = write!(w, "movsx ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, true, wide, has_rex, opsz16) { pos += c; }
                }
                0xBF => {
                    let _ = write!(w, "movsx ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let _ = write!(w, "{}", reg64(reg, ext_r));
                    let _ = write!(w, ",");
                    if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, false, wide, has_rex, true) { pos += c; }
                }

                // -- XADD --
                0xC0 | 0xC1 => {
                    let byte_sz = ext == 0xC0;
                    let _ = write!(w, "xadd ");
                    if !rest.is_empty() {
                        let modrm = rest[0];
                        let r = (modrm >> 3) & 7;
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) {
                            let _ = write!(w, ",");
                            write_reg(w, r, byte_sz, wide, has_rex, ext_r, opsz16);
                            pos += c;
                        }
                    }
                }

                // -- MOVNTI --
                0xC3 => {
                    let _ = write!(w, "movnti ");
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    let reg = (modrm >> 3) & 7;
                    let c = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16)?;
                    pos += c;
                    let _ = write!(w, ",{}", reg64(reg, ext_r));
                }

                // -- CMPXCHG8B / CMPXCHG16B --
                0xC7 => {
                    if rest.is_empty() { return Some(pos); }
                    let modrm = rest[0];
                    if ((modrm >> 3) & 7) == 1 {
                        if wide {
                            let _ = write!(w, "cmpxchg16b ");
                        } else {
                            let _ = write!(w, "cmpxchg8b ");
                        }
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; }
                    } else {
                        let _ = write!(w, "?");
                        if let Some(c) = write_ea(w, rest, addrsz32, ext_b, ext_x, byte_sz, wide, has_rex, opsz16) { pos += c; }
                    }
                }

                // -- BSWAP --
                0xC8..=0xCF => {
                    let r = ext - 0xC8;
                    let _ = write!(w, "bswap {}", reg64(r, ext_b));
                }

                // -- Unknown two-byte opcode --
                _ => {
                    let _ = write!(w, "db {:02x} {:02x}", 0x0F, ext);
                    // Try to consume the ModRM byte: most two-byte opcodes
                    // that reach here take a ModRM operand.
                    if !rest.is_empty() {
                        pos += 1;
                    }
                }
            }
        }

        // ===========================================================
        // Unknown single-byte opcode
        // ===========================================================
        _ => {}
    }

    Some(pos)
}
