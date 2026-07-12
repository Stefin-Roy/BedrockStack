#!/usr/bin/env python3
"""Create a GPT-partitioned FAT32 disk image for BedrockOS.

Thin wrapper around build_image.py's GPT image creation.
"""

import os
import sys

WORKSPACE = os.path.dirname(os.path.abspath(__file__))
TARGET_DIR = os.path.join(WORKSPACE, "target")


def main():
    print("Creating GPT+FAT32 disk image...")

    boot_path = os.path.join(TARGET_DIR, "x86_64-unknown-uefi", "debug", "boot.efi")
    if not os.path.exists(boot_path):
        boot_path = os.path.join(TARGET_DIR, "x86_64-unknown-uefi", "release", "boot.efi")
    kernel_path = os.path.join(TARGET_DIR, "x86_64-unknown-none", "debug", "kernel")
    if not os.path.exists(kernel_path):
        kernel_path = os.path.join(TARGET_DIR, "x86_64-unknown-none", "release", "kernel")

    for name, path in [("boot.efi", boot_path), ("kernel", kernel_path)]:
        if not os.path.exists(path):
            print(f"ERROR: {name} not found at {path}")
            sys.exit(1)

    from build_image import create_gpt_image, TARGET_DIR as _td
    create_gpt_image(boot_path, kernel_path)


if __name__ == "__main__":
    main()
