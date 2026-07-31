#!/usr/bin/env python3
"""Generate a raw MBR disk image for a demo USB drive.

Layout: one primary FAT32 partition (type 0x0C, LBA-partitioned) starting at
LBA 2048, formatted with a `Yo` directory containing `Yo/wassup.txt`
("Hello, World! <smiley>").  Written so BedrockOS's partition probe + FAT32
driver can mount it on real hardware.

Formatting uses WSL (Ubuntu) with mkfs.fat + mtools, mirroring create_image.py.
"""

import argparse
import os
import struct
import subprocess

WORKSPACE = os.path.dirname(os.path.abspath(__file__))
TARGET_DIR = os.path.join(WORKSPACE, "target")
SECTOR = 512
SECTORS_PER_MB = 1024 * 1024 // SECTOR
WSL_DISTRO = "Ubuntu"
DEFAULT_OUTPUT = os.path.join(TARGET_DIR, "demo.img")
DEFAULT_DISK_MB = 64
PART_START_LBA = 2048
DEFAULT_LABEL = "BEDROCKOS"


def to_wsl(path):
    path = path.replace("\\", "/")
    if len(path) >= 2 and path[1] == ":":
        return f"/mnt/{path[0].lower()}{path[2:]}"
    return path


def run_wsl(script):
    subprocess.run(
        ["wsl.exe", "-d", WSL_DISTRO, "--", "bash", "-c", script],
        check=True,
    )


def make_demo(output, disk_mb, label):
    os.makedirs(TARGET_DIR, exist_ok=True)

    disk_sectors = disk_mb * SECTORS_PER_MB
    part_sectors = disk_sectors - PART_START_LBA
    part_mb = part_sectors // SECTORS_PER_MB

    print(f"Creating {disk_mb} MB MBR disk image...")
    print(f"  partition: LBA {PART_START_LBA}..{disk_sectors - 1} ({part_mb} MB, type 0x0C FAT32)")

    disk = bytearray(disk_sectors * SECTOR)
    # One primary partition entry: status 0x80, type 0x0C (FAT32 LBA).
    disk[446:462] = struct.pack(
        "<BBBBBBBBII", 0x80, 0, 0, 0, 0x0C, 0xFF, 0xFF, 0xFF, PART_START_LBA, part_sectors
    )
    disk[510:512] = b"\x55\xAA"

    part_img = os.path.join(TARGET_DIR, "demo_part.img")
    demo_txt = os.path.join(TARGET_DIR, "demo_wassup.txt")
    with open(demo_txt, "wb") as f:
        f.write("Hello, World! \U0001F60A\n".encode("utf-8"))

    print("  Formatting FAT32 partition and adding Yo/wassup.txt (WSL)...")
    run_wsl(
        "set -euo pipefail; "
        f"dd if=/dev/zero of='{to_wsl(part_img)}' bs=1M count={part_mb}; "
        f"mkfs.fat -F 32 -n {label} '{to_wsl(part_img)}'; "
        f"mmd -i '{to_wsl(part_img)}' ::/Yo; "
        f"mcopy -i '{to_wsl(part_img)}' '{to_wsl(demo_txt)}' ::/Yo/wassup.txt; "
        f"echo '--- listing Yo ---'; mdir -i '{to_wsl(part_img)}' ::/Yo; "
        f"echo '--- Yo/wassup.txt ---'; mtype -i '{to_wsl(part_img)}' ::/Yo/wassup.txt"
    )

    print("  Splicing partition into disk image...")
    with open(part_img, "rb") as f:
        part_data = f.read(part_sectors * SECTOR)
    disk[PART_START_LBA * SECTOR:PART_START_LBA * SECTOR + len(part_data)] = part_data

    with open(output, "wb") as f:
        f.write(bytes(disk))
    print(f"  Image: {output}")

    print("  Verifying partition table + FAT32 VBR...")
    verify(disk)

    os.remove(part_img)
    os.remove(demo_txt)


def verify(disk):
    mbr = disk[446:462]
    status, ptype = mbr[0], mbr[4]
    start = struct.unpack("<I", mbr[8:12])[0]
    size = struct.unpack("<I", mbr[12:16])[0]
    print(f"    MBR entry: status=0x{status:02x} type=0x{ptype:02x} start_lba={start} size={size}")
    boot = disk[start * SECTOR:start * SECTOR + 512]
    oem = boot[3:11]
    bps = struct.unpack("<H", boot[0x0B:0x0D])[0]
    fat_sz32 = struct.unpack("<I", boot[0x24:0x28])[0]
    sig = "OK" if boot[510:512] == b"\x55\xAA" else "BAD"
    print(f"    VBR: oem={oem!r} bytes_per_sec={bps} fat_sz32={fat_sz32} sig={sig}")


def main():
    parser = argparse.ArgumentParser(description="Generate a demo FAT32 USB disk image")
    parser.add_argument("--output", default=DEFAULT_OUTPUT, help="output disk image path")
    parser.add_argument("--size", type=int, default=DEFAULT_DISK_MB, help="disk size in MB")
    parser.add_argument("--label", default=DEFAULT_LABEL, help="FAT32 volume label")
    args = parser.parse_args()
    make_demo(args.output, args.size, args.label)
    print("Done!")


if __name__ == "__main__":
    main()
