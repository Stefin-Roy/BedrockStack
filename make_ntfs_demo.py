#!/usr/bin/env python3
"""Generate a raw MBR disk image with an NTFS partition for the read-only
NTFS driver selftest.

Layout: one primary NTFS partition (type 0x07) starting at LBA 2048,
formatted with mkfs.ntfs and populated with ntfscp/ntfsmkdir (both from the
Ubuntu `ntfs-3g` package, installed on demand — same WSL pattern as
make_demo_drive.py and create_image.py).

Contents:
  Yo/wassup.txt        "Hello from NTFS! (read-only demo)"
  Yo/big.bin           5 MiB pattern file (byte i = i % 251)
  Yo/empty.txt         empty
  Yo/uni-<U+540D>.txt   UTF-16 name exercise
  Yo/emptydir/         empty directory
  Yo/nested/deep/      nested directory walk
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
DEFAULT_OUTPUT = os.path.join(TARGET_DIR, "ntfs.img")
DEFAULT_DISK_MB = 64
PART_START_LBA = 2048
DEFAULT_LABEL = "BEDROCKNTFS"

BIG_BIN_MB = 5


def to_wsl(path):
    path = path.replace("\\", "/")
    if len(path) >= 2 and path[1] == ":":
        return f"/mnt/{path[0].lower()}{path[2:]}"
    return path


def run_wsl(script):
    # `-u root`: WSL's root has no password, so ntfs-3g can be installed and
    # used without interactive sudo (the default Ubuntu user needs one).
    subprocess.run(
        ["wsl.exe", "-d", WSL_DISTRO, "-u", "root", "--", "bash", "-c", script],
        check=True,
    )


def make_demo(output, disk_mb, label):
    os.makedirs(TARGET_DIR, exist_ok=True)

    disk_sectors = disk_mb * SECTORS_PER_MB
    part_sectors = disk_sectors - PART_START_LBA
    part_mb = part_sectors // SECTORS_PER_MB

    print(f"Creating {disk_mb} MB MBR disk image...")
    print(f"  partition: LBA {PART_START_LBA}..{disk_sectors - 1} ({part_mb} MB, type 0x07 NTFS)")

    disk = bytearray(disk_sectors * SECTOR)
    disk[446:462] = struct.pack(
        "<BBBBBBBBII", 0x80, 0, 0, 0, 0x07, 0xFF, 0xFF, 0xFF, PART_START_LBA, part_sectors
    )
    disk[510:512] = b"\x55\xAA"

    part_img = os.path.join(TARGET_DIR, "ntfs_part.img")
    wassup_txt = os.path.join(TARGET_DIR, "ntfs_wassup.txt")
    big_bin = os.path.join(TARGET_DIR, "ntfs_big.bin")
    empty_txt = os.path.join(TARGET_DIR, "ntfs_empty.txt")
    uni_txt = os.path.join(TARGET_DIR, "ntfs_uni.txt")

    with open(wassup_txt, "wb") as f:
        f.write(b"Hello from NTFS! (read-only demo)\n")
    with open(big_bin, "wb") as f:
        chunk = bytearray(65536)
        total = BIG_BIN_MB * 1024 * 1024
        pos = 0
        while pos < total:
            n = min(65536, total - pos)
            for j in range(n):
                chunk[j] = (pos + j) % 251
            f.write(chunk[:n])
            pos += n
    with open(empty_txt, "wb") as f:
        pass
    with open(uni_txt, "wb") as f:
        f.write("unicode name file\n".encode("utf-8"))

    print("  Formatting NTFS partition and populating (WSL, root)...")
    run_wsl(
        "set -euo pipefail; "
        "if ! command -v mkfs.ntfs >/dev/null 2>&1; then "
        "apt-get update -qq && apt-get install -y -qq ntfs-3g; fi; "
        f"dd if=/dev/zero of='{to_wsl(part_img)}' bs=1M count={part_mb}; "
        f"mkfs.ntfs -F -Q -L {label} '{to_wsl(part_img)}'; "
        f"mkdir -p /root/ntdemo; "
        f"mount -o loop '{to_wsl(part_img)}' /root/ntdemo; "
        f"mkdir -p /root/ntdemo/Yo/nested/deep /root/ntdemo/Yo/emptydir; "
        f"cp '{to_wsl(wassup_txt)}' /root/ntdemo/Yo/wassup.txt; "
        f"cp '{to_wsl(big_bin)}' /root/ntdemo/Yo/big.bin; "
        f"cp '{to_wsl(empty_txt)}' /root/ntdemo/Yo/empty.txt; "
        f"cp '{to_wsl(uni_txt)}' /root/ntdemo/Yo/uni-\u540d.txt; "
        f"sync; umount /root/ntdemo; "
        f"echo '--- / ---'; ntfsls -p / '{to_wsl(part_img)}'; "
        f"echo '--- /Yo ---'; ntfsls -p /Yo '{to_wsl(part_img)}'"
    )

    print("  Splicing partition into disk image...")
    with open(part_img, "rb") as f:
        part_data = f.read(part_sectors * SECTOR)
    disk[PART_START_LBA * SECTOR:PART_START_LBA * SECTOR + len(part_data)] = part_data

    with open(output, "wb") as f:
        f.write(bytes(disk))
    print(f"  Image: {output}")

    print("  Verifying partition table + NTFS boot sector...")
    verify(disk)

    for p in (part_img, wassup_txt, big_bin, empty_txt, uni_txt):
        os.remove(p)


def verify(disk):
    mbr = disk[446:462]
    status, ptype = mbr[0], mbr[4]
    start = struct.unpack("<I", mbr[8:12])[0]
    size = struct.unpack("<I", mbr[12:16])[0]
    print(f"    MBR entry: status=0x{status:02x} type=0x{ptype:02x} start_lba={start} size={size}")
    boot = disk[start * SECTOR:start * SECTOR + 512]
    oem = boot[3:11]
    bps = struct.unpack("<H", boot[0x0B:0x0D])[0]
    spc = boot[0x0D]
    mft_lcn = struct.unpack("<Q", boot[0x30:0x38])[0]
    sig = "OK" if boot[510:512] == b"\x55\xAA" else "BAD"
    print(
        f"    VBR: oem={oem!r} bytes_per_sec={bps} sec_per_clus={spc} mft_lcn={mft_lcn} sig={sig}"
    )


def main():
    parser = argparse.ArgumentParser(description="Generate a demo NTFS disk image")
    parser.add_argument("--output", default=DEFAULT_OUTPUT, help="output disk image path")
    parser.add_argument("--size", type=int, default=DEFAULT_DISK_MB, help="disk size in MB")
    parser.add_argument("--label", default=DEFAULT_LABEL, help="NTFS volume label")
    args = parser.parse_args()
    make_demo(args.output, args.size, args.label)
    print("Done!")


if __name__ == "__main__":
    main()
