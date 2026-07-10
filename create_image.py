#!/usr/bin/env python3
"""Create a GPT-partitioned FAT32 disk image for BedrockOS.

Uses WSL (Ubuntu) with mtools + mkfs.fat for reliable FAT32 creation.
GPT code adapted from LodaxOS (proven working).
"""

import binascii
import os
import struct
import subprocess
import sys

SECTOR = 512
ESP_GUID = bytes([0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B])
WSL_DISTRO = "Ubuntu"
DISK_MB = 64
ESP_MB = 60
ESP_FIRST_SECTOR = 2048

WORKSPACE = os.path.dirname(os.path.abspath(__file__))
TARGET_DIR = os.path.join(WORKSPACE, "target")
OUTPUT_IMG = os.path.join(TARGET_DIR, "os.img")


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


def main():
    os.makedirs(TARGET_DIR, exist_ok=True)

    boot_path = os.path.join(TARGET_DIR, "x86_64-unknown-uefi", "debug", "boot.efi")
    if not os.path.exists(boot_path):
        boot_path = os.path.join(TARGET_DIR, "x86_64-unknown-uefi", "release", "boot.efi")
    kernel_path = os.path.join(TARGET_DIR, "x86_64-unknown-none", "debug", "kernel")

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
    esp_img_wsl = to_wsl(os.path.join(TARGET_DIR, "esp_part.img"))

    run_wsl(
        "set -euo pipefail; "
        f"dd if=/dev/zero of='{esp_img_wsl}' bs=1M count={ESP_MB}; "
        f"mkfs.fat -F 32 -n BEDROCKOS '{esp_img_wsl}'; "
        f"mmd -i '{esp_img_wsl}' ::/EFI; "
        f"mmd -i '{esp_img_wsl}' ::/EFI/BOOT; "
        f"mmd -i '{esp_img_wsl}' ::/EFI/BEDROCK; "
        f"mcopy -i '{esp_img_wsl}' '{boot_wsl}' ::/EFI/BOOT/BOOTX64.EFI; "
        f"mcopy -i '{esp_img_wsl}' '{kernel_wsl}' ::/EFI/BEDROCK/KERNEL; "
        f"mdir -i '{esp_img_wsl}' ::/EFI/BOOT; "
        f"mdir -i '{esp_img_wsl}' ::/EFI/BEDROCK"
    )

    print("  Splicing ESP into disk image...")
    with open(os.path.join(TARGET_DIR, "esp_part.img"), "rb") as f:
        esp_data = f.read(esp_sectors * SECTOR)
    disk[esp_first * SECTOR:esp_first * SECTOR + len(esp_data)] = esp_data
    print(f"  ESP: {len(esp_data) // 1024} KB written")

    with open(OUTPUT_IMG, "wb") as f:
        written = f.write(bytes(disk))
    print(f"  Image: {OUTPUT_IMG} ({written} bytes)")
    print("Done.")


if __name__ == "__main__":
    print("Creating GPT+FAT32 disk image...")
    main()
