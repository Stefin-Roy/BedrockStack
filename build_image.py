#!/usr/bin/env python3
"""Build the OS disk image."""

import os
import shutil
import struct
import subprocess
import sys

WORKSPACE = os.path.dirname(os.path.abspath(__file__))
TARGET_DIR = os.path.join(WORKSPACE, "target")
BUILD_DIR = os.path.join(TARGET_DIR, "image")
ESP_DIR = os.path.join(BUILD_DIR, "EFI", "BOOT")
OUTPUT_IMG = os.path.join(TARGET_DIR, "os.img")

QEMU_PATH = r"C:\Program Files\qemu\qemu-system-x86_64.exe"
OVMF_CODE = r"C:\Program Files\qemu\share\edk2-x86_64-code.fd"
OVMF_VARS = r"C:\Program Files\qemu\share\edk2-x86_64-vars.fd"


def profile_dir(profile):
    """Map a cargo --profile name to its target output directory name."""
    return "debug" if profile == "dev" else profile


def find_kernel(profile):
    """Locate the built kernel binary for the given profile."""
    candidate = os.path.join(TARGET_DIR, "x86_64-unknown-none", profile_dir(profile), "kernel")
    if os.path.exists(candidate):
        return candidate
    # Fall back to the other common profile directory.
    for alt in ("debug", "release"):
        candidate = os.path.join(TARGET_DIR, "x86_64-unknown-none", alt, "kernel")
        if os.path.exists(candidate):
            return candidate
    return None


def run(cmd, **kwargs):
    print(f"  {' '.join(cmd)}")
    result = subprocess.run(cmd, cwd=WORKSPACE, **kwargs)
    if result.returncode != 0:
        print(f"  ERROR: command failed with exit code {result.returncode}")
        sys.exit(1)
    return result


def build(target, profile, packages=None):
    cmd = ["cargo", "build", "--target", target, "--profile", profile]
    if packages:
        for p in packages:
            cmd.extend(["-p", p])
    run(cmd)


def create_fat32_image(efi_binary_path, profile):
    """Create a minimal FAT32 image with the EFI binary."""
    # For now, just create a raw image and copy the EFI binary
    # The user will need to manually create the FAT32 structure
    # or install mtools

    os.makedirs(BUILD_DIR, exist_ok=True)

    # Check if mtools is available
    try:
        subprocess.run(["mtools", "--version"], capture_output=True, check=True)
        return create_fat32_with_mtools(efi_binary_path, profile)
    except FileNotFoundError:
        pass

    # Check if mkfs.fat is available
    try:
        subprocess.run(["mkfs.fat", "--version"], capture_output=True, check=True)
        return create_fat32_with_mkfs(efi_binary_path, profile)
    except FileNotFoundError:
        pass

    # Fallback: just copy the EFI binary
    print("  WARNING: mtools and mkfs.fat not found")
    print("  Copying EFI binary to target directory")
    shutil.copy2(efi_binary_path, os.path.join(TARGET_DIR, "BOOTX64.EFI"))
    print(f"  EFI binary: {os.path.join(TARGET_DIR, 'BOOTX64.EFI')}")
    return None


def create_fat32_with_mtools(efi_binary_path, profile):
    """Create FAT32 image using mtools."""
    if os.path.exists(OUTPUT_IMG):
        os.remove(OUTPUT_IMG)

    run(["qemu-img", "create", "-f", "raw", OUTPUT_IMG, "64M"])
    run(["mkfs.fat", "-F", "32", "-n", "BEDROCKOS", OUTPUT_IMG])
    run(["mmd", "-i", OUTPUT_IMG, "::EFI"])
    run(["mmd", "-i", OUTPUT_IMG, "::EFI/BOOT"])
    run(["mmd", "-i", OUTPUT_IMG, "::EFI/BEDROCK"])
    run(["mcopy", "-i", OUTPUT_IMG, efi_binary_path, "::EFI/BOOT/BOOTX64.EFI"])
    kernel_path = find_kernel(profile)
    if kernel_path:
        run(["mcopy", "-i", OUTPUT_IMG, kernel_path, "::/EFI/BEDROCK/KERNEL"])
        print(f"  Kernel copied: {kernel_path}")
    else:
        print("  WARNING: Kernel binary not found")
    print(f"  Image created: {OUTPUT_IMG}")
    return OUTPUT_IMG


def create_fat32_with_mkfs(efi_binary_path, profile):
    """Create FAT32 image using mkfs.fat."""
    if os.path.exists(OUTPUT_IMG):
        os.remove(OUTPUT_IMG)

    # Create a directory structure for the FAT image
    fat_dir = os.path.join(BUILD_DIR, "fat")
    efi_boot_dir = os.path.join(fat_dir, "EFI", "BOOT")
    efi_bedrock_dir = os.path.join(fat_dir, "EFI", "BEDROCK")
    os.makedirs(efi_boot_dir, exist_ok=True)
    os.makedirs(efi_bedrock_dir, exist_ok=True)
    shutil.copy2(efi_binary_path, os.path.join(efi_boot_dir, "BOOTX64.EFI"))

    kernel_path = find_kernel(profile)
    if kernel_path:
        shutil.copy2(kernel_path, os.path.join(efi_bedrock_dir, "KERNEL"))
        print(f"  Kernel copied: {kernel_path}")

    # Create a blank image
    run(["qemu-img", "create", "-f", "raw", OUTPUT_IMG, "64M"])

    # Format as FAT32
    run(["mkfs.fat", "-F", "32", "-n", "BEDROCKOS", OUTPUT_IMG])

    # Try to copy files using mcopy if available
    try:
        run(["mcopy", "-i", OUTPUT_IMG, "-s", fat_dir + "/.", "::/"])
        print(f"  Image created: {OUTPUT_IMG}")
        return OUTPUT_IMG
    except SystemExit:
        print("  mcopy not available, image created but empty")
        return OUTPUT_IMG


def main():
    import argparse
    parser = argparse.ArgumentParser(description="Build BedrockOS")
    parser.add_argument("--profile", choices=["dev", "release"], default="dev")
    parser.add_argument("--no-run", action="store_true")
    args = parser.parse_args()

    profile = args.profile

    print("Building boot crate (UEFI)...")
    build("x86_64-unknown-uefi", profile, ["boot"])

    print("Building kernel crate...")
    build("x86_64-unknown-none", profile, ["kernel"])

    print("Creating disk image...")
    uefi_bin = os.path.join(
        TARGET_DIR, "x86_64-unknown-uefi", profile_dir(profile), "boot.efi"
    )
    if not os.path.exists(uefi_bin):
        print(f"ERROR: UEFI binary not found at {uefi_bin}")
        sys.exit(1)

    create_fat32_image(uefi_bin, profile)

    if not args.no_run:
        print("\nRunning QEMU...")
        # Use split OVMF code+vars via pflash (consistent with run.bat /
        # fullrun.bat). The vars file must be a writable copy of the matching
        # template — a fabricated blank file would not match the code image.
        vars_copy = os.path.join(TARGET_DIR, "ovmf_vars.fd")
        shutil.copy2(OVMF_VARS, vars_copy)
        qemu_cmd = [
            QEMU_PATH,
            "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}",
            "-drive", f"if=pflash,format=raw,file={vars_copy}",
            "-drive", f"format=raw,file={OUTPUT_IMG}",
            "-m", "256M",
            "-nographic",
            "-serial", "mon:stdio",
        ]
        run(qemu_cmd)

    print("Done!")


if __name__ == "__main__":
    main()
