#!/usr/bin/env python3
"""
Prune vendored Rust dependencies in third_party/dependencies.

Removes known-not-needed files (tests, benches, docs, CI, *.stderr, Cargo.lock, *.md, etc.)
and patches each .cargo-checksum.json so `cargo build --offline --frozen` still works.

Safe to re-run. Idempotent.

Usage:
  python tools/prune-vendor.py [--check] [--vendor third_party/dependencies]
  --check: dry-run, exit 1 if pruning would change anything
"""

import argparse
import json
import os
import pathlib
import shutil
import sys

VENDOR_DEFAULT = pathlib.Path("third_party/dependencies")

# Directories to prune entirely (matched as top-level prefix or any segment)
# NOTE: keep `doc`/`docs` — crates like `bitvec` embed `doc/*.md` via `include_str!` in src.
PRUNE_DIRS = {
    "tests",
    "test",
    "benches",
    "bench",
    "benchmarks",
    "benchmark",
    "examples",
    "example",
    "fuzz",
    "fuzzer",
    "testdata",
    "testing",
    ".github",
    ".gitlab",
    "ci",
    ".ci",
    "website",
    "book",
    "etc",
    "xtask",
    "tools",  # some crates vendor tooling scripts
    "scripts",
    "assets",
}

# Exact file names to prune (case-sensitive)
PRUNE_EXACT = {
    "Cargo.lock",
    ".cargo_vcs_info.json",
    "Cargo.toml.orig",  # cargo uses Cargo.toml (generated); keep size down
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".dockerignore",
    "Dockerfile",
    "Makefile",
    "BUILD.bazel",
    "MODULE.bazel",
    "appveyor.yml",
    ".travis.yml",
    ".cirrus.yml",
    "cirrus.yml",
    "rust-toolchain.toml",
    "rust-toolchain",
    ".rustfmt.toml",
    "rustfmt.toml",
    ".clippy.toml",
    "clippy.toml",
}

PRUNE_EXTS = {
    ".stderr",
    ".orig",  # catches Cargo.toml.orig already, but also generic
}

# Extensions that are mostly noise inside vendor crates (safe because `src/` keeps .rs)
# We do NOT prune *.md by default — some crates embed README/example.md via `include_str!("../README.md")`
# (e.g. ptr_meta-0.3.1/src/lib.rs:74 `include_str!("../example.md")`). Deleting those breaks
# `cargo build`. Keep all .md; only prune via directory rules (tests/ etc.).
PRUNE_MD = False  # keep *.md


def should_prune(rel: pathlib.Path) -> bool:
    # rel is posix relative path inside crate dir, e.g. "tests/ui/foo.stderr"
    parts = rel.parts
    # directory prefix check
    for p in parts[:-1] if len(parts) > 1 else []:
        if p in PRUNE_DIRS:
            return True
    # top-level dir check
    if len(parts) >= 1 and parts[0] in PRUNE_DIRS:
        return True

    name = parts[-1] if parts else ""
    # exact name
    if name in PRUNE_EXACT:
        return True
    # Cargo.toml.orig is already in exact, but generic .orig
    if name.endswith(".orig"):
        return True
    # .stderr anywhere
    if rel.suffix == ".stderr":
        return True
    # yml/yaml CI files (anywhere)
    if rel.suffix in (".yml", ".yaml"):
        return True
    # md files — keep all (needed for include_str! in some crates)
    if PRUNE_MD and rel.suffix == ".md":
        low = name.lower()
        if low.startswith("license") or low.startswith("licence") or low.startswith("copying") or low.startswith("unlicense"):
            return False
        return True
    # If PRUNE_MD is False, never prune .md here
    # rustfmt/clippy etc already handled, but generic hidden config
    if name in (".rustfmt.toml", ".clippy.toml"):
        return True
    return False


def prune_one(crate_dir: pathlib.Path, check: bool) -> tuple[int, int]:
    removed = 0
    patched = False

    checksum_path = crate_dir / ".cargo-checksum.json"
    files_in_checksum = {}
    if checksum_path.is_file():
        try:
            with open(checksum_path, "r", encoding="utf-8") as f:
                data = json.load(f)
                files_in_checksum = data.get("files", {})
        except Exception as e:
            print(f"[prune] warn: cannot parse {checksum_path}: {e}", file=sys.stderr)
            files_in_checksum = {}

    to_delete: list[pathlib.Path] = []
    for root, _dirs, files in os.walk(crate_dir, topdown=True):
        rel_root = pathlib.Path(root).relative_to(crate_dir)
        # file-level prune — matches any file whose relative path contains a PRUNE_DIRS component
        for fn in files:
            rel = pathlib.Path(root).relative_to(crate_dir) / fn if rel_root != pathlib.Path(".") else pathlib.Path(fn)
            # normalize to posix for matching
            rel_posix = pathlib.Path(rel.as_posix())
            if should_prune(rel_posix):
                to_delete.append(pathlib.Path(root) / fn)

    # also handle empty files that match prune patterns with no dir
    # dedupe
    to_delete = sorted(set(to_delete))

    # delete
    for p in to_delete:
        if check:
            removed += 1
            continue
        try:
            p.unlink()
            removed += 1
        except FileNotFoundError:
            pass
        except Exception as e:
            print(f"[prune] warn: cannot delete {p}: {e}", file=sys.stderr)

    # remove empty dirs (bottom-up) that were pruned
    if not check:
        for root, dirs, files in os.walk(crate_dir, topdown=False):
            for d in dirs:
                dp = pathlib.Path(root) / d
                try:
                    # only remove if empty
                    if not any(dp.iterdir()):
                        dp.rmdir()
                except Exception:
                    pass

    # patch checksum: drop entries for deleted files (and for any file that no longer exists but was in checksum)
    if checksum_path.is_file():
        try:
            with open(checksum_path, "r", encoding="utf-8") as f:
                data = json.load(f)
        except Exception:
            return removed, 0
        files = data.get("files", {})
        # build set of posix relatives that were deleted or no longer exist
        deleted_posix = {p.relative_to(crate_dir).as_posix().replace("\\", "/") for p in to_delete}
        # also any checksum entry whose file no longer exists on disk (covers already-deleted via previous run)
        for key in list(files.keys()):
            # checksum keys are posix path inside crate (e.g. "tests/ui/foo.stderr")
            if key in deleted_posix or not (crate_dir / key).exists():
                # only drop if file is pruned (i.e., not a needed file that was accidentally deleted)
                # we consider any missing file that matches prune pattern as intentional prune,
                # otherwise keep entry to avoid hiding accidental deletions
                pp = pathlib.Path(key)
                if should_prune(pp) or key in deleted_posix:
                    del files[key]
                    patched = True
                # else: file missing but not in prune set — leave entry so cargo may warn
        if patched:
            if check:
                pass
            else:
                data["files"] = files
                with open(checksum_path, "w", encoding="utf-8", newline="\n") as f:
                    json.dump(data, f, indent=4)
                    f.write("\n")
    return removed, 1 if patched else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="dry run, exit 1 if changes needed")
    ap.add_argument("--vendor", default=str(VENDOR_DEFAULT), help="vendor directory")
    args = ap.parse_args()

    vendor = pathlib.Path(args.vendor)
    if not vendor.is_dir():
        print(f"[prune] vendor dir not found: {vendor}", file=sys.stderr)
        sys.exit(2)

    total_removed = 0
    total_patched = 0
    crates = [d for d in vendor.iterdir() if d.is_dir()]
    crates.sort()
    for crate_dir in crates:
        removed, patched = prune_one(crate_dir, check=args.check)
        total_removed += removed
        total_patched += patched
        if removed or patched:
            print(f"[prune] {crate_dir.name}: removed {removed} files, patched checksum={bool(patched)}")

    if args.check and (total_removed or total_patched):
        print(f"[prune] --check: would remove {total_removed} files, patch {total_patched} checksums", file=sys.stderr)
        sys.exit(1)
    print(f"[prune] done: removed {total_removed} files, patched {total_patched} crates, vendor={vendor}")

if __name__ == "__main__":
    main()
