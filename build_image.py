#!/usr/bin/env python3
"""Build the OS disk image."""

import binascii
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


def find_user(profile):
    """Locate the built user INIT binary for the given profile."""
    candidate = os.path.join(TARGET_DIR, "x86_64-bedrock-user", profile_dir(profile), "user")
    if os.path.exists(candidate):
        return candidate
    # Fall back to the other common profile directory.
    for alt in ("debug", "release"):
        candidate = os.path.join(TARGET_DIR, "x86_64-bedrock-user", alt, "user")
        if os.path.exists(candidate):
            return candidate
    # Fall back to the legacy shared x86_64-unknown-none location.
    for alt in (profile_dir(profile), "debug", "release"):
        candidate = os.path.join(TARGET_DIR, "x86_64-unknown-none", alt, "user")
        if os.path.exists(candidate):
            return candidate
    return None


def find_doom(profile):
    """Locate the built DOOM game binary for the given profile."""
    candidate = os.path.join(TARGET_DIR, "x86_64-bedrock-user", profile_dir(profile), "doom")
    if os.path.exists(candidate):
        return candidate
    # Fall back to the other common profile directory.
    for alt in ("debug", "release"):
        candidate = os.path.join(TARGET_DIR, "x86_64-bedrock-user", alt, "doom")
        if os.path.exists(candidate):
            return candidate
    return None


def find_wad():
    """Locate the Freedoom IWAD (optional; the image still builds without it)."""
    wad = os.path.join(WORKSPACE, "data", "freedoom1.wad")
    if os.path.exists(wad):
        return wad
    return None


def run(cmd, **kwargs):
    print(f"  {' '.join(cmd)}")
    result = subprocess.run(cmd, cwd=WORKSPACE, **kwargs)
    if result.returncode != 0:
        print(f"  ERROR: command failed with exit code {result.returncode}")
        sys.exit(1)
    return result


def build(target, profile, packages=None, features=None):
    cmd = ["cargo", "build", "--target", target, "--profile", profile]
    if features:
        cmd.extend(["--features", features])
    if packages:
        for p in packages:
            cmd.extend(["-p", p])
    run(cmd)


def create_fat32_image(efi_binary_path, profile):
    """Create a minimal FAT32 image with the EFI binary."""
    # Prefer mtools, then mkfs.fat; only fall back to a raw copy when neither
    # is installed. Both tools build the FAT32 structure (and copy the kernel)
    # automatically.

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
    user_path = find_user(profile)
    if user_path:
        run(["mcopy", "-i", OUTPUT_IMG, user_path, "::/EFI/BEDROCK/INIT"])
        print(f"  INIT (user) copied: {user_path}")
    else:
        print("  WARNING: user INIT binary not found")
    doom_path = find_doom(profile)
    if doom_path:
        run(["mcopy", "-i", OUTPUT_IMG, doom_path, "::/EFI/BEDROCK/DOOM"])
        print(f"  DOOM game copied: {doom_path}")
    else:
        print("  WARNING: DOOM binary not found")
    wad_path = find_wad()
    if wad_path:
        run(["mcopy", "-i", OUTPUT_IMG, wad_path, "::/EFI/BEDROCK/FREEDOOM.WAD"])
        print(f"  Freedoom IWAD copied: {wad_path}")
    else:
        print("  WARNING: data/freedoom1.wad not found - DOOM will need an IWAD")
    wav_path = os.path.join(WORKSPACE, "Sounds", "startup.wav")
    if os.path.exists(wav_path):
        run(["mcopy", "-i", OUTPUT_IMG, wav_path, "::/EFI/BEDROCK/STARTUP.WAV"])
        print(f"  Startup chime copied: {wav_path}")
    else:
        print("  WARNING: Sounds/startup.wav not found - no startup chime on image")
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

    user_path = find_user(profile)
    if user_path:
        shutil.copy2(user_path, os.path.join(efi_bedrock_dir, "INIT"))
        print(f"  INIT (user) copied: {user_path}")
    else:
        print("  WARNING: user INIT binary not found")

    doom_path = find_doom(profile)
    if doom_path:
        shutil.copy2(doom_path, os.path.join(efi_bedrock_dir, "DOOM"))
        print(f"  DOOM game copied: {doom_path}")
    else:
        print("  WARNING: DOOM binary not found")

    wad_path = find_wad()
    if wad_path:
        shutil.copy2(wad_path, os.path.join(efi_bedrock_dir, "FREEDOOM.WAD"))
        print(f"  Freedoom IWAD copied: {wad_path}")
    else:
        print("  WARNING: data/freedoom1.wad not found - DOOM will need an IWAD")

    wav_path = os.path.join(WORKSPACE, "Sounds", "startup.wav")
    if os.path.exists(wav_path):
        shutil.copy2(wav_path, os.path.join(efi_bedrock_dir, "STARTUP.WAV"))
        print(f"  Startup chime copied: {wav_path}")
    else:
        print("  WARNING: Sounds/startup.wav not found - no startup chime on image")

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


SECTOR = 512
ESP_GUID = bytes([0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B])
WSL_DISTRO = "Ubuntu"
DISK_MB = 64
ESP_MB = 60
ESP_FIRST_SECTOR = 2048


def crc32(data):
    return binascii.crc32(data) & 0xFFFFFFFF


def to_wsl(path):
    path = path.replace("\\", "/")
    if len(path) >= 2 and path[1] == ":":
        return f"/mnt/{path[0].lower()}{path[2:]}"
    return path


def run_wsl(script):
    try:
        subprocess.run(
            ["wsl.exe", "-d", WSL_DISTRO, "--", "bash", "-c", script],
            check=True,
        )
    except subprocess.CalledProcessError as e:
        print(f"\n[ERROR] WSL command failed with exit code {e.returncode}.", file=sys.stderr)
        print("Ensure packages 'e2fsprogs' and 'mtools' are installed in WSL.", file=sys.stderr)
        raise e


def part_entry(type_guid, unique_guid, first, last, name):
    entry = bytearray(128)
    entry[0:16] = type_guid
    entry[16:32] = unique_guid
    struct.pack_into("<Q", entry, 32, first)
    struct.pack_into("<Q", entry, 40, last)
    struct.pack_into("<Q", entry, 48, (last - first + 1) * SECTOR)
    encoded = name.encode("utf-16-le")
    entry[56:56 + len(encoded)] = encoded
    return entry


def write_gpt(disk, disk_sectors, esp_first, esp_last):
    entries_lba = 2
    disk_guid = os.urandom(16)
    entries = bytearray(128 * 128)
    entries[0:128] = part_entry(ESP_GUID, os.urandom(16), esp_first, esp_last, "EFI System Partition")
    disk[entries_lba * SECTOR:entries_lba * SECTOR + len(entries)] = entries

    hdr = bytearray(SECTOR)
    hdr[0:8] = b"EFI PART"
    struct.pack_into("<I", hdr, 8, 0x00010000)
    struct.pack_into("<I", hdr, 12, 92)
    struct.pack_into("<Q", hdr, 24, 1)
    struct.pack_into("<Q", hdr, 32, disk_sectors - 1)
    struct.pack_into("<Q", hdr, 40, 34)
    struct.pack_into("<Q", hdr, 48, disk_sectors - 34)
    hdr[56:72] = disk_guid
    struct.pack_into("<Q", hdr, 72, entries_lba)
    struct.pack_into("<I", hdr, 80, 128)
    struct.pack_into("<I", hdr, 84, 128)
    struct.pack_into("<I", hdr, 88, crc32(bytes(entries)))
    struct.pack_into("<I", hdr, 16, crc32(bytes(hdr[:92])))
    disk[SECTOR:2 * SECTOR] = hdr

    backup_entries_lba = disk_sectors - 33
    disk[backup_entries_lba * SECTOR:backup_entries_lba * SECTOR + len(entries)] = entries
    backup_hdr = bytearray(hdr)
    struct.pack_into("<Q", backup_hdr, 24, disk_sectors - 1)
    struct.pack_into("<Q", backup_hdr, 32, 1)
    struct.pack_into("<Q", backup_hdr, 72, backup_entries_lba)
    struct.pack_into("<I", backup_hdr, 88, crc32(bytes(entries)))
    # The header CRC must be computed with the CRC field zeroed. backup_hdr was
    # copied from the primary header, which already has its CRC set, so clear it
    # first — otherwise the backup GPT header CRC is invalid.
    struct.pack_into("<I", backup_hdr, 16, 0)
    struct.pack_into("<I", backup_hdr, 16, crc32(bytes(backup_hdr[:92])))
    disk[(disk_sectors - 1) * SECTOR:disk_sectors * SECTOR] = backup_hdr


def create_gpt_image(boot_path, kernel_path):
    """Create a GPT-partitioned FAT32 disk image using WSL."""
    os.makedirs(TARGET_DIR, exist_ok=True)

    for name, path in [("boot.efi", boot_path), ("kernel", kernel_path)]:
        if not os.path.exists(path):
            print(f"ERROR: {name} not found at {path}")
            sys.exit(1)

    print(f"  boot.efi: {os.path.getsize(boot_path)} bytes")
    print(f"  kernel:   {os.path.getsize(kernel_path)} bytes")

    esp_sectors = ESP_MB * 1024 * 1024 // SECTOR
    disk_sectors = DISK_MB * 1024 * 1024 // SECTOR
    esp_first = ESP_FIRST_SECTOR
    esp_last = esp_first + esp_sectors - 1

    print(f"Creating {DISK_MB} MB GPT disk image...")
    print(f"  ESP: LBA {esp_first}..{esp_last}")

    disk = bytearray(disk_sectors * SECTOR)
    disk[446:462] = struct.pack("<BBBBBBBBII", 0, 0, 0, 1, 0xEE, 0xFF, 0xFF, 0xFF, 1, disk_sectors - 1)
    disk[510:512] = b"\x55\xAA"
    write_gpt(disk, disk_sectors, esp_first, esp_last)
    print("  GPT written (primary + backup)")

    print("  Formatting ESP with mkfs.fat via WSL...")
    boot_wsl = to_wsl(boot_path)
    kernel_wsl = to_wsl(kernel_path)
    user_path = find_user("dev")
    esp_img_wsl = to_wsl(os.path.join(TARGET_DIR, "esp_part.img"))

    if user_path:
        user_init_copy = f"mcopy -i '{esp_img_wsl}' '{to_wsl(user_path)}' ::/EFI/BEDROCK/INIT; "
    else:
        print("  WARNING: user INIT binary not found - skipping /EFI/BEDROCK/INIT")
        user_init_copy = ""

    doom_path = find_doom("dev")
    if doom_path:
        doom_copy = f"mcopy -i '{esp_img_wsl}' '{to_wsl(doom_path)}' ::/EFI/BEDROCK/DOOM; "
    else:
        print("  WARNING: DOOM binary not found - skipping /EFI/BEDROCK/DOOM")
        doom_copy = ""

    wad_path = find_wad()
    if wad_path:
        wad_copy = f"mcopy -i '{esp_img_wsl}' '{to_wsl(wad_path)}' ::/EFI/BEDROCK/FREEDOOM.WAD; "
    else:
        print("  WARNING: data/freedoom1.wad not found - DOOM will need an IWAD")
        wad_copy = ""

    wav_path = os.path.join(WORKSPACE, "Sounds", "startup.wav")
    if os.path.exists(wav_path):
        wav_wsl = to_wsl(wav_path)
        wav_copy = f"mcopy -i '{esp_img_wsl}' '{wav_wsl}' ::/EFI/BEDROCK/STARTUP.WAV; "
    else:
        print("  WARNING: Sounds/startup.wav not found - no startup chime on ESP")
        wav_copy = ""

    run_wsl(
        "set -euo pipefail; "
        f"dd if=/dev/zero of='{esp_img_wsl}' bs=1M count={ESP_MB}; "
        f"mkfs.fat -F 32 -n BEDROCKOS '{esp_img_wsl}'; "
        f"mmd -i '{esp_img_wsl}' ::/EFI; "
        f"mmd -i '{esp_img_wsl}' ::/EFI/BOOT; "
        f"mmd -i '{esp_img_wsl}' ::/EFI/BEDROCK; "
        f"mcopy -i '{esp_img_wsl}' '{boot_wsl}' ::/EFI/BOOT/BOOTX64.EFI; "
        f"mcopy -i '{esp_img_wsl}' '{kernel_wsl}' ::/EFI/BEDROCK/KERNEL; "
        + user_init_copy
        + doom_copy
        + wad_copy
        + wav_copy
        + f"mdir -i '{esp_img_wsl}' ::/EFI/BOOT; "
        + f"mdir -i '{esp_img_wsl}' ::/EFI/BEDROCK"
    )

    print("  Splicing ESP into disk image...")
    with open(os.path.join(TARGET_DIR, "esp_part.img"), "rb") as f:
        esp_data = f.read(esp_sectors * SECTOR)
    disk[esp_first * SECTOR:esp_first * SECTOR + len(esp_data)] = esp_data
    print(f"  ESP: {len(esp_data) // 1024} KB written")

    with open(OUTPUT_IMG, "wb") as f:
        written = f.write(bytes(disk))
    print(f"  Image: {OUTPUT_IMG} ({written} bytes)")


def main():
    import argparse
    parser = argparse.ArgumentParser(description="Build BedrockOS")
    parser.add_argument("--profile", choices=["dev", "release"], default="dev")
    parser.add_argument("--no-run", action="store_true")
    parser.add_argument("--gpt", action="store_true", help="Create GPT-partitioned image (requires WSL)")
    parser.add_argument("--skip-build", action="store_true", help="Skip cargo build, use existing binaries")
    args = parser.parse_args()

    profile = args.profile
    uefi_bin = os.path.join(TARGET_DIR, "x86_64-unknown-uefi", profile_dir(profile), "boot.efi")
    kernel_bin = os.path.join(TARGET_DIR, "x86_64-unknown-none", profile_dir(profile), "kernel")

    if not args.skip_build:
        print("Building boot crate (UEFI)...")
        build("x86_64-unknown-uefi", profile, ["boot"], features="cpu_slow")
        print("Building kernel crate...")
        build("x86_64-unknown-none", profile, ["kernel"])

    if not os.path.exists(uefi_bin):
        print(f"ERROR: UEFI binary not found at {uefi_bin}")
        sys.exit(1)

    if args.gpt:
        print("Creating GPT disk image...")
        create_gpt_image(uefi_bin, kernel_bin)
    else:
        print("Creating FAT32 disk image...")
        create_fat32_image(uefi_bin, profile)

    if not args.no_run:
        print("\nRunning QEMU...")
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
