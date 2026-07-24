# Display / Framebuffer — Invariants

**Version:** 0.5.0
**Source:** `graphics/Framebuffer/src/{display,framebuffer,console}.rs`
**Status:** Stable

---

## State Invariants

**DISP-001 — Framebuffer pointer has only `bpp > 0` assertion:**
`Framebuffer::new()` asserts `bpp > 0` for non-zero addresses.
Zero address (no display) is allowed. The `addr % bpp == 0` assertion was
removed because GRUB framebuffer tags may report `bpp_bits == 0` and the
alignment check could spuriously fail on some firmware.
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

**DISP-011 — Shadow buffer for cached drawing:**
`Framebuffer` allocates a cacheable RAM shadow buffer (`shadow: *mut u8`)
from the kernel page allocator as contiguous physical pages. All drawing
primitives (`put_pixel`, `fill_rect`, `clear`, `draw_glyph_raw`) write to
the shadow buffer via regular cached stores (packed `u32` values), never
directly to the real scanout framebuffer.
- Location: `graphics/Framebuffer/src/framebuffer.rs`

**DISP-012 — Dirty rectangle tracking:**
Each drawing primitive expands a dirty bounding box (`dirty_x1`, `dirty_y1`,
`dirty_x2`, `dirty_y2`) via `mark_dirty()`. The dirty region coalesces
multiple small writes (e.g. a line of text glyphs) into a single
rectangular copy on the next `flush()`.
- Location: `graphics/Framebuffer/src/framebuffer.rs`

**DISP-013 — Deferred flushing to real framebuffer:**
Drawing primitives never flush automatically. `flush()` copies only the
dirty scanlines from shadow to real fb via `core::ptr::copy_nonoverlapping`
(bulk memcpy). `flush_full()` copies the entire buffer. `scroll_up()` and
`clear()` mark the full screen dirty and call `flush()` inline.
- Location: `graphics/Framebuffer/src/framebuffer.rs`

**DISP-014 — Console delegates to `Display` trait:**
`Console` stores a `*mut dyn Display` (lifetime-erased via `core::mem::transmute`)
instead of duplicating `fb_ptr`, `stride`, `bpp`, `width`, `height`, and
`pixel_format`. All drawing (`draw_char`, `scroll_up`, `clear`, `flush`)
delegates through the `Display` trait interface.
- Location: `graphics/Framebuffer/src/console.rs`

**DISP-015 — Console flushes once per string output:**
`Console::puts()` writes the entire string then calls `Display::flush()` once.
`Console::putc_and_flush()` provides single-character immediate visibility.
- Location: `graphics/Framebuffer/src/console.rs`

**DISP-016 — `Framebuffer::new()` requires shadow address:**
`shadow_addr: u64` parameter provides the physical address of the shadow
buffer. `shadow_ptr()` and `shadow_slice()`/`shadow_slice_mut()` expose the
shadow for direct access.
- Location: `graphics/Framebuffer/src/framebuffer.rs`

---

## Safety Invariants

**DISP-S001 — `Framebuffer::new()` safety:**
Both `addr` (real framebuffer) and `shadow_addr` (shadow buffer) must be
valid for `stride * height * bpp` bytes of writable memory each.
- Location: `graphics/Framebuffer/src/framebuffer.rs:17-19`

**DISP-S002 — `draw_glyph_raw()` safety:**
Writes packed `u32` values to `buf` (the shadow buffer, not the real fb).
The computed offset is validated by bounds checks. The write is unsafe
because it dereferences the raw pointer.
- Location: `graphics/Framebuffer/src/framebuffer.rs:192-201`

**DISP-S003 — `clear()` safety:**
Zeroes `stride * height * bpp` bytes in the shadow buffer via
`core::ptr::write_bytes` (fast memset). Null pointer is a safe no-op.
- Location: `graphics/Framebuffer/src/framebuffer.rs:143-151`

**DISP-S004 — `Framebuffer` is `!Sync`:**
Access is single-threaded (only BSP writes to the display). No `Sync`
impl is provided, preventing data races.
- Location: `graphics/Framebuffer/src/framebuffer.rs` (implicit)

**DISP-S005 — `Console` delegates through `Display` trait:**
Console stores `*mut dyn Display` (lifetime-erased via `transmute`) and
calls trait methods for all drawing. The caller must ensure the `Display`
outlives the `Console`.
- Location: `graphics/Framebuffer/src/console.rs`

**DISP-S006 — `phys_addr()` / `shadow_phys_addr()` return raw addresses:**
Return the framebuffer and shadow buffer physical addresses for page-table
mapping. The caller must ensure the addresses are still valid when used.
- Location: `graphics/Framebuffer/src/framebuffer.rs:39-41`

---

## API Contracts

**DISP-API-001 — `Framebuffer::new(addr, width, height, stride, pixel_format, bpp, shadow_addr)`:**
Returns a `Framebuffer` ready for drawing. Panics if `bpp == 0` or
`width > stride`. The `shadow_addr` must point to a writable buffer of
`stride * height * bpp` bytes.

**DISP-API-002 — `Display` trait:**
Provides:
- `draw_char(x, y, ch) → bool`
- `put_pixel(x, y, color) → bool`
- `fill_rect(x, y, w, h, color)`
- `scroll_up(rows)`
- `clear()`
- `flush()` (default no-op)
- `width() → usize`
- `height() → usize`

**DISP-API-003 — `Framebuffer::flush()` / `flush_full()`:**
`flush()` copies only the dirty rectangle scanlines from shadow buffer to
real framebuffer via `core::ptr::copy_nonoverlapping`. `flush_full()`
copies the entire buffer. Both reset the dirty flag.

**DISP-API-004 — `Console`:**
Wraps a `&mut dyn Display` pointer for cursor-based text output. All
drawing delegates to the trait interface. Default colors are white-on-black;
`set_colors(fg, bg)` overrides them. `puts()` flushes once after writing
the entire string. `putc_and_flush()` for single-character visibility.

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
- Drawing primitives pack pixel data as `u32` via `Color::to_pixel_u32()`
  and write to the shadow buffer with regular non-volatile stores. Only
  `flush()` uses `copy_nonoverlapping` to transfer the dirty region to the
  real scanout buffer. This is safe because the shadow buffer is in
  cacheable RAM, not device MMIO.
- The shadow buffer is allocated from the kernel page allocator as
  contiguous physical pages. Its physical address is passed to
  `Framebuffer::new()` as `shadow_addr`.
- On x86_64, the real framebuffer pages are mapped with `WRITE_COMBINING`
  (PAT entry 1 = 01h) instead of `NO_CACHE`, enabling the CPU to coalesce
  flush stores into burst writes over the PCIe bus. APIC and other MMIO
  regions remain `NO_CACHE`.
