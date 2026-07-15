# Display / Framebuffer — Invariants

**Version:** 0.4.0
**Source:** `graphics/Framebuffer/src/{display,framebuffer,console}.rs`
**Status:** Stable

---

## State Invariants

**DISP-001 — Framebuffer pointer is `bpp`-aligned:**
`Framebuffer::new()` asserts `addr % bpp == 0` for non-zero addresses.
Zero address (no display) is allowed.
- Location: `graphics/Framebuffer/src/framebuffer.rs:18-20`

**DISP-002 — `width <= stride` (pixels per scanline):**
UEFI GOP reports stride in pixels, which may exceed width.
- Location: `graphics/Framebuffer/src/framebuffer.rs:21`

**DISP-003 — `draw_char()` bounds-checks all coordinates:**
Returns `false` (no-op) if `x >= width`, `y >= height`, or `ch >= 128`.
Row/col loops break early at framebuffer edges.
- Location: `graphics/Framebuffer/src/framebuffer.rs:69-74`

**DISP-004 — Pixel format is respected when writing:**
`Bgr` writes Blue→Green→Red order. `Rgb` writes Red→Green→Blue order.
Both use `bpp` bytes per pixel.
- Location: `graphics/Framebuffer/src/color.rs:30-33`

**DISP-005 — `put_pixel()` bounds-checks coordinates:**
Returns `false` if `x >= width` or `y >= height`.
- Location: `graphics/Framebuffer/src/framebuffer.rs:89-91`

**DISP-006 — `fill_rect()` clips to framebuffer edges:**
Row/col loops break early at width/height boundaries.
- Location: `graphics/Framebuffer/src/framebuffer.rs:100-122`

**DISP-007 — `scroll_up()` preserves visible content:**
Copies `(height - rows) * stride * bpp` bytes upward, then zeroes the
vacated rows at the bottom. If `rows >= height`, clears the entire buffer.
- Location: `graphics/Framebuffer/src/framebuffer.rs:124-141`

**DISP-008 — `Console` stores foreground/background colors:**
Defaults to white-on-black. `set_colors()` updates both for subsequent
character draws.
- Location: `graphics/Framebuffer/src/console.rs:41-43`

**DISP-009 — `bpp` parameter is consistent:**
`Framebuffer`, `Console`, and paging all receive the same `bpp` value.
`FramebufferInfo.bpp` is set by the bootloader (default 4 for UEFI GOP).
- Location: `common/src/types.rs:38`, `boot/src/main.rs:254`

**DISP-010 — Scrollback buffer (feature = "scrollback"):**
Console maintains a `screen_chars` buffer tracking the current on-screen
characters. On scroll, the top line is pushed into `scrollback` before
the framebuffer is shifted. `scroll_back()`/`scroll_forward()`/`reset_scroll()`
redraw from the scrollback buffer.
- Location: `graphics/Framebuffer/src/console.rs` (feature-gated)

---

## Safety Invariants

**DISP-S001 — `Framebuffer::new()` safety:**
`addr` must be valid for `stride * height * bpp` bytes of writable memory.
- Location: `graphics/Framebuffer/src/framebuffer.rs:17-19`

**DISP-S002 — `draw_char()` / `draw_glyph_raw()` safety:**
The computed offset `py * stride * bpp + px * bpp` is validated by the
bounds checks. The write is unsafe because it dereferences the raw pointer.
- Location: `graphics/Framebuffer/src/framebuffer.rs:192-201`

**DISP-S003 — `clear()` safety:**
Zeroes `stride * height * bpp` bytes starting at `ptr`. Null pointer
is a safe no-op.
- Location: `graphics/Framebuffer/src/framebuffer.rs:143-151`

**DISP-S004 — `Framebuffer` is `!Sync`:**
Access is single-threaded (only BSP writes to the display). No `Sync`
impl is provided, preventing data races.
- Location: `graphics/Framebuffer/src/framebuffer.rs` (implicit)

**DISP-S005 — `Console` uses shared `draw_glyph_raw`:**
Console delegates pixel manipulation to `draw_glyph_raw` instead of
duplicating the rendering loop. The same safety invariants apply.
- Location: `graphics/Framebuffer/src/console.rs:53-70`

**DISP-S006 — `phys_addr()` returns raw address:**
Returns the framebuffer physical address for page-table mapping.
The caller must ensure the address is still valid when used.
- Location: `graphics/Framebuffer/src/framebuffer.rs:39-41`

---

## API Contracts

**DISP-API-001 — `Framebuffer::new(addr, width, height, stride, pixel_format, bpp)`:**
Returns a `Framebuffer` ready for drawing. Panics if `addr != 0` and not
`bpp`-aligned, or if `width > stride`.

**DISP-API-002 — `Display` trait:**
Provides:
- `draw_char(x, y, ch) → bool`
- `put_pixel(x, y, color) → bool`
- `fill_rect(x, y, w, h, color)`
- `scroll_up(rows)`
- `clear()`
- `width() → usize`
- `height() → usize`

**DISP-API-003 — `Console`:**
Wraps a raw framebuffer pointer for cursor-based text output. Uses
`draw_glyph_raw` for character rendering and `core::ptr::copy` for scroll.
Default colors are white-on-black; `set_colors(fg, bg)` overrides them.

With feature `scrollback` enabled, the last `SCROLLBACK_LINES` (1024) of
scrolled-off content are available via `scroll_back()`, `scroll_forward()`,
and `reset_scroll()`.

---

## Design Notes

- The built-in font (`FONT`) has exactly 128 entries (ASCII 0-127), each
  16 bytes (8x16 bitmap). It is stored in `.rodata` and never mutated.
- Control characters (0-31) are all blank (all-zero glyphs).
- `bpp` defaults to 4 (32bpp) for UEFI GOP. Future display drivers may
  use different values (e.g. 3 for 24bpp).
- `Color` provides named constants (`WHITE`, `BLACK`, `RED`, etc.) and
  format-aware `to_pixel_bytes()` for BGR/RGB byte ordering.
- The `draw_glyph_raw` function is `pub(crate)` to both `Framebuffer` and
  `Console`, eliminating the code duplication that existed in v0.2.0.
- Scrollback stores raw character bytes in a flat `Vec<u8>`. Each line
  contributes `max_cols` bytes. The buffer is capped at `SCROLLBACK_LINES`
  lines (oldest lines are dropped when full).
