# NOTICE — GPL-2.0+ linkage

This crate (`user/doom`) is the BedrockOS port of the **doomgeneric** engine,
vendored under `third_party/doomgeneric/`.  That engine is derived from
id Software's original DOOM source and Simon Howard's Chocolate Doom, and is
licensed under the **GNU General Public License, version 2 or later**
(GPL-2.0+); every engine source file carries that license header.  The full
GPL v2 text is available from the Free Software Foundation.

By linking this crate against that engine, the resulting `doom` program and
this crate's own glue (`platform/doomgeneric_bedrock.c`,
`platform/shim.c`, `include/`) are treated as a combined work licensed under
GPL-2.0+, in accordance with the engine's license terms.

The following BedrockOS components are **not** part of this GPL work and are
deliberately isolated from it (they are linked against, but their licenses
are unaffected):

- `user/libc` (Rust, permissively licensed) — provides `malloc`, `fopen`/
  `fread`/`fseek`, `printf`, string helpers, and `errno` to the engine via the
  C ABI.  It contains no engine code and no GPL-derived material.
- `kernel/`, `common/`, `graphics/`, `boot/` — the operating system.
- `user/doom/src/main.rs` — the C-ABI bridge between the OS and the engine.

The engine's `DG_*` interface (`doomgeneric.h`) is the documented integration
boundary; all BedrockOS-specific code lives behind it.
