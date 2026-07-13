# Display / Framebuffer — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/display/{mod,framebuffer}.rs`
**Status:** Stable

---

## State Invariants

**DISP-001 — Framebuffer pointer is non-null and 4-byte aligned:**
`Framebuffer::new()` asserts `addr % 4 == 0` for non-zero addresses.
Zero address (no display) is allowed.
- Location: `kernel/src/display/framebuffer.rs:32-35`

**DISP-002 — `width <= stride` (pixels per scanline):**
UEFI GOP reports stride in pixels, which may exceed width.
- Location: `kernel/src/display/framebuffer.rs:42`

**DISP-003 — `draw_char()` bounds-checks all coordinates:**
Returns `false` (no-op) if `x >= width`, `y >= height`, or `ch >= 128`.
Row/col loops break early at framebuffer edges.
- Location: `kernel/src/display/framebuffer.rs:86-101`

**DISP-004 — Pixel format is respected when writing:**
`Bgr` writes Blue→Green→Red order. `Rgb` writes Red→Green→Blue order.
Both use 32 bits per pixel (8 bits reserved).
- Location: `kernel/src/display/framebuffer.rs:107-128`

---

## Safety Invariants

**DISP-S001 — `Framebuffer::new()` safety:**
`addr` must be valid for `stride * height * 4` bytes of writable memory.
- Location: `kernel/src/display/framebuffer.rs:27-30`

**DISP-S002 — `draw_char()` safety:**
The computed offset `py * stride * 4 + px * 4` is validated by the
bounds checks to be less than `stride * height * 4`. The write is
unsafe because it dereferences the raw framebuffer pointer.
- Location: `kernel/src/display/framebuffer.rs:106-129`

**DISP-S003 — `clear()` safety:**
Zeroes `stride * height * 4` bytes starting at `ptr`. Null pointer
is a safe no-op.
- Location: `kernel/src/display/framebuffer.rs:136-141`

**DISP-S004 — `Framebuffer` is `!Sync`:**
Access is single-threaded (only BSP writes to the display). No `Sync`
impl is provided, preventing data races.
- Location: `kernel/src/display/framebuffer.rs` (implicit — no `unsafe impl Sync`)

---

## API Contracts

**DISP-API-001 — `Framebuffer::new(addr, width, height, stride, pixel_format)`:**
Returns a `Framebuffer` ready for drawing. Panics if `addr != 0` and not
4-byte aligned, or if `width > stridet`.

**DISP-API-002 — `Display` trait:**
Provides `draw_char(x, y, ch) → bool`, `clear()`, `width()`, `height()`.

---

## Design Notes

- The built-in font (`FONT`) has exactly 128 entries (ASCII 0-127), each
  16 bytes (8x16 bitmap). It is stored in `.rodata` and never mutated.
- Control characters (0-31) are all blank (all-zero glyphs).
- The framebuffer is 32bpp (4 bytes per pixel) regardless of pixel format.
