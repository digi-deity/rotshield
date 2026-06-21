#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "crc32c>=2.2.post0",
#   "btrfs-recon",
# ]
#
# [tool.uv.sources]
# btrfs-recon = { path = "/root/btrfs-recon" }
# ///

"""
Scans the dmesg buffer (or a kernel log file) for BTRFS checksum failures,
extracts the relevant inode and offset information, and attempts to recover
the corrupted data using parity information from the array.

Handles two kernel message formats:

  Read-I/O failure (btrfs detects corruption during a normal file read):
    BTRFS warning (device nmd1p1): csum failed root 5 ino 257 off 167936 \
      csum 0x28f86fd2 expected csum 0xf8d99c3c mirror 1

  Scrub failure (btrfs detects corruption during a background scrub):
    BTRFS warning (device nmd1p1): checksum error at logical ... \
      root 5, inode 257, offset 167936, length 4096
"""

from __future__ import annotations
import argparse
import os
import re
import sys
import subprocess
from crc32c import crc32c
from btrfs_recon.constants import BTRFS_SECTOR_SIZE
from btrfs_recon.parsing import (
    parse_superblock,
    build_chunk_tree,
    find_tree_root,
    find_extent_data,
    lookup_csum,
)
from btrfs_recon.structure import ObjectId
from md_array import get_array_config, must_be_root

DEDUP_WINDOW_SECONDS = 5  # minimum seconds between processing the same (dev, ino, off) triplet

# The btrfs scrub (via the NonRAID driver) reports the start of the 32 KB-aligned
# window that contains the corrupt sector, not the exact 4 KB sector address.
# Scan this many sectors (8 × 4 KiB = 32 KiB) forward from the reported offset
# to locate the actually-corrupt sector.
SCAN_WINDOW_SECTORS = 8

def find_physical_offset(mount_dev: str, target_ino: int, target_off: int):
    with open(mount_dev, 'rb') as fp:
        superblock = parse_superblock(fp)
        devid_fp_map = {superblock.dev_item.devid: fp}
        tree = build_chunk_tree(superblock, devid_fp_map)

        try:
            fs_root = find_tree_root(superblock, tree, devid_fp_map, ObjectId.FsTree)
            found_item = find_extent_data(tree, devid_fp_map, fs_root, target_ino, target_off)
        except KeyError as e:
            raise RuntimeError(str(e))

        logical_addr = found_item.data.ref.disk_bytenr + (target_off - found_item.key.offset)
        chunks = list(tree.at(logical_addr))
        if not chunks:
            raise RuntimeError(f"No chunk mapping for logical address {logical_addr}")

        chunk = chunks[0]
        phys_base = chunk.data['stripes'][0][1]
        abs_phys_offset = phys_base + (logical_addr - chunk.begin)
        devid = chunk.data['stripes'][0][0]

        return devid, abs_phys_offset, logical_addr

def xor_bytes(data_list: list[bytes]) -> bytes:
    result = bytearray(data_list[0])
    for next_block in data_list[1:]:
        for i in range(len(result)):
            result[i] ^= next_block[i]
    return bytes(result)

def lookup_extent_csum(mount_dev: str, logical_addr: int) -> int:
    """Look up the stored CRC32C for the 4 KiB sector containing logical_addr."""
    sector_logical = (logical_addr // BTRFS_SECTOR_SIZE) * BTRFS_SECTOR_SIZE
    with open(mount_dev, 'rb') as fp:
        superblock = parse_superblock(fp)
        devid_fp_map = {superblock.dev_item.devid: fp}
        tree = build_chunk_tree(superblock, devid_fp_map)
        try:
            csum_root = find_tree_root(superblock, tree, devid_fp_map, ObjectId.CsumTree)
            return lookup_csum(fp, tree, csum_root, sector_logical)
        except KeyError as e:
            raise RuntimeError(str(e))

# --- Field detection -----------------------------------------------------
# Different btrfs/kernel versions reorder fields, rename them (ino vs inode,
# off vs offset), and even insert extra tokens between the device tag and
# the marker phrase (e.g. "scrub: " in newer scrub messages). So instead of
# one anchored sequential pattern, we treat every piece of information as
# an independent search over the line:
#   1. pull the device out of the "(device X):" prefix
#   2. classify the line type by searching for a marker phrase anywhere in it
#   3. pull each field out by searching for it anywhere in it
# Nothing assumes adjacency or ordering between these.

WARNING_PREFIX = re.compile(r'BTRFS warning \(device (?P<dev>\S+)\):')

LINE_TYPE_MARKERS = [
    ('read-io', re.compile(r'\bcsum failed\b')),
    ('scrub',   re.compile(r'\bchecksum error at\b')),
]

FIELD_PATTERNS = {
    'root':   re.compile(r'\broot[\s=]+(\d+)'),
    'ino':    re.compile(r'\b(?:ino|inode)[\s=]+(\d+)'),
    'off':    re.compile(r'\b(?:off|offset)[\s=]+(\d+)'),
    # Reported checksum only appears on read-io failures, as "csum 0x<hex>".
    'actual': re.compile(r'\bcsum\s+0x([0-9a-f]+)'),
}

REQUIRED_FIELDS = {'ino', 'off'}  # everything else is optional / informational


def _extract_fields(line: str) -> dict | None:
    """Parse a single log line into a field dict, independent of field
    order, field naming, and extra tokens inserted between known parts."""
    prefix_m = WARNING_PREFIX.search(line)
    if not prefix_m:
        return None

    label = next((name for name, marker in LINE_TYPE_MARKERS if marker.search(line)), None)
    if label is None:
        return None  # a BTRFS warning, but not one we care about

    fields = {'type': label, 'dev': prefix_m.group('dev'), 'raw': line.strip()}
    for name, pattern in FIELD_PATTERNS.items():
        fm = pattern.search(line)
        if fm:
            fields[name] = fm.group(1)

    missing = REQUIRED_FIELDS - fields.keys()
    if missing:
        print(f"[!] Skipping unparsable {label} line (missing {', '.join(sorted(missing))}): {line.strip()}")
        return None

    return fields


def monitor_dmesg(log_file: str | None = None):
    """
    Reads the current dmesg buffer (or a log file), processes all BTRFS csum
    failures, and then exits.
    """
    if log_file:
        print(f"Scanning log file '{log_file}' for BTRFS csum failures...")
        try:
            with open(log_file, 'r') as f:
                log_data = f.read()
        except OSError as e:
            sys.exit(f"ERROR: Failed to read log file: {e}")
    else:
        print(f"Scanning dmesg for BTRFS csum failures...")
        try:
            log_data = subprocess.check_output(['dmesg'], text=True)
        except subprocess.CalledProcessError as e:
            sys.exit(f"ERROR: Failed to read dmesg: {e}")

    found_any = False
    seen_errors = set()

    for line in log_data.splitlines():
        if 'BTRFS warning' not in line:
            continue

        fields = _extract_fields(line)
        if fields is None:
            continue

        error_key = (fields['dev'], fields['ino'], fields['off'])
        if error_key in seen_errors:
            continue
        seen_errors.add(error_key)
        found_any = True

        print(f"\n[!] Detected {fields['type']} corruption: {fields['raw']}")
        handle_failure(fields)

    if not found_any:
        print("No BTRFS checksum failures found in dmesg.")
    else:
        print(f"\nFinished processing {len(seen_errors)} unique errors.")


def handle_failure(fields: dict):
    must_be_root()
    failing_dev_name = fields['dev']
    ino, reported_off = int(fields['ino']), int(fields['off'])
    actual_csum_str = fields.get('actual')
    actual_csum_reported = int(actual_csum_str, 16) if actual_csum_str else None

    mount_dev = f"/dev/{failing_dev_name}" if not failing_dev_name.startswith('/') else failing_dev_name
    config = get_array_config()

    # --- Locate the corrupt sector -------------------------------------------
    # The btrfs scrub (via the NonRAID driver) reports the start of a 32 KB-
    # aligned window as the inode offset rather than the exact 4 KB sector.
    # Scan forward up to SCAN_WINDOW_SECTORS to find the sector whose on-disk
    # CRC32C no longer matches the value stored in the CSUM_TREE.
    devid = phys_offset = logical_addr = expected_csum = None
    corrupted_block = failing_path = None

    print(f"Scanning {SCAN_WINDOW_SECTORS} sectors from reported offset {reported_off}...")
    for i in range(SCAN_WINDOW_SECTORS):
        candidate_off = reported_off + i * BTRFS_SECTOR_SIZE
        try:
            d, p, la = find_physical_offset(mount_dev, ino, candidate_off)
        except RuntimeError as e:
            print(f"  [{i}] offset {candidate_off}: no extent — {e}")
            continue

        try:
            ec = lookup_extent_csum(mount_dev, la)
        except RuntimeError as e:
            print(f"  [{i}] offset {candidate_off} → logical 0x{la:x}: no stored csum — {e}")
            continue

        fp_path = config.data_devs.get(d)
        if not fp_path:
            print(f"  [{i}] offset {candidate_off}: DevID {d} not in array config")
            continue

        with open(fp_path, 'rb') as f:
            f.seek(p)
            block = f.read(BTRFS_SECTOR_SIZE)

        disk_csum = crc32c(block)
        if disk_csum != ec:
            print(f"  [{i}] offset {candidate_off} → phys 0x{p:x}: CORRUPT "
                  f"(disk=0x{disk_csum:08x}, stored=0x{ec:08x})")
            devid, phys_offset, logical_addr, expected_csum = d, p, la, ec
            corrupted_block, failing_path = block, fp_path
            break

        print(f"  [{i}] offset {candidate_off} → phys 0x{p:x}: OK (csum=0x{disk_csum:08x})")

    if devid is None:
        print(f"✓ No corruption found in {SCAN_WINDOW_SECTORS}-sector window — skipping recovery.")
        return

    print(f"\nFailing Device: {failing_path} (DevID: {devid}) | Offset: 0x{phys_offset:x}")
    print(f"  On-disk csum:   0x{crc32c(corrupted_block):08x}")
    print(f"  Metadata csum:  0x{expected_csum:08x}")
    print("✓ Corruption confirmed: on-disk data does not match metadata checksum.")

    # For read-I/O failures the log also carries the csum btrfs actually read.
    # Cross-check it against our own read to confirm we found the right block.
    if actual_csum_reported is not None:
        disk_csum = crc32c(corrupted_block)
        print(f"  Reported actual csum: 0x{actual_csum_reported:08x}")
        print(f"  Our read csum:        0x{disk_csum:08x}")
        if disk_csum != actual_csum_reported:
            raise RuntimeError(
                f"Block at 0x{phys_offset:x}: our read csum 0x{disk_csum:08x} "
                f"differs from the csum btrfs reported (0x{actual_csum_reported:08x}). "
                "Aborting — we may be looking at the wrong location."
            )
        print("  ✓ Matches dmesg-reported csum.")

    blocks_to_xor = []
    for slot, path in config.data_devs.items():
        if path == failing_path:
            continue
        with open(path, 'rb') as f:
            f.seek(phys_offset)
            blocks_to_xor.append(f.read(BTRFS_SECTOR_SIZE))

    with open(config.parity_p, 'rb') as f:
        f.seek(phys_offset)
        blocks_to_xor.append(f.read(BTRFS_SECTOR_SIZE))

    recovered_data = xor_bytes(blocks_to_xor)
    computed_csum = crc32c(recovered_data)

    print(f"Expected Csum: 0x{expected_csum:08x} | Recovered: 0x{computed_csum:08x}")
    if computed_csum == expected_csum:
        print("✓ SUCCESS: Data recovered!")
    else:
        print("✗ FAILURE: Checksum still mismatch.")

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='Recover BTRFS corrupted blocks using parity.')
    parser.add_argument(
        '--log-file', metavar='FILE',
        help='Path to a kernel log file to scan instead of running dmesg.'
    )
    args = parser.parse_args()
    monitor_dmesg(log_file=args.log_file)