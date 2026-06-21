#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "crc32c>=2.2.post0",
#   "btrfs-recon",
#   "construct",
#   "construct-typing",
#   "intervaltree",
#   "loguru"
# ]
#
# [tool.uv.sources]
# btrfs-recon = { path = "/root/project/unraid-btrfs-integrity-recovery" }
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
from loguru import logger

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

# 1. Remove the default logger configuration
logger.remove()
logger.add(sys.stdout, format="{time:HH:mm:ss} - {level: <8} - {message}")

DEDUP_WINDOW_SECONDS = 5  # minimum seconds between processing the same (dev, ino, off) triplet

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


def find_all_corrupt_sectors(
    mount_dev: str,
    data_dev_path: str,
    base_logical: int,
    base_phys: int,
    window: int = 64,
) -> list[tuple[int, int]]:
    """Scan ±window sectors around base_logical/base_phys for CRC32C mismatches.

    Returns a list of (logical, phys) for every mismatching sector in the window.
    The scrub reports the start of a chunk window, not the exact sector, so there
    may be more than one corrupt sector within this range.
    """
    with open(mount_dev, 'rb') as fp:
        superblock = parse_superblock(fp)
        devid_fp_map = {superblock.dev_item.devid: fp}
        tree = build_chunk_tree(superblock, devid_fp_map)
        try:
            csum_root = find_tree_root(superblock, tree, devid_fp_map, ObjectId.CsumTree)
        except KeyError as e:
            raise RuntimeError(str(e))

        mismatches = []
        for delta in range(-window, window + 1):
            logical = base_logical + delta * BTRFS_SECTOR_SIZE
            phys    = base_phys    + delta * BTRFS_SECTOR_SIZE
            if phys < 0:
                continue
            with open(data_dev_path, 'rb') as df:
                df.seek(phys)
                data = df.read(BTRFS_SECTOR_SIZE)
            actual = crc32c(data)
            try:
                stored = lookup_csum(fp, tree, csum_root, logical)
            except Exception:
                continue
            if actual != stored:
                mismatches.append((logical, phys))
        return mismatches

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

def write_recovered_data(
    recovered_data: bytes,
    phys_offset: int,
    failing_path: str,
) -> None:
    """Write recovered data to disk, bypassing the array layer so parity is not updated."""
    logger.info("✓ Checksum verified! Writing recovered data to disk...")
    with open(failing_path, 'r+b') as f:
        f.seek(phys_offset)
        f.write(recovered_data)
        f.flush()
        os.fsync(f.fileno())
    logger.info(f"✓ SUCCESS: Data written to {failing_path} at offset 0x{phys_offset:x}")


def recover_sector(
    mount_dev: str,
    config,
    failing_path: str,
    logical_addr: int,
    phys_offset: int,
    actual_csum_reported: int | None = None,
) -> dict:
    """Attempt parity recovery for a single confirmed-corrupt sector.

    actual_csum_reported: the CRC32C btrfs logged (read-IO failures only), used
    to cross-check that we are reading the exact sector btrfs complained about.
    Only meaningful when logical_addr matches the originally reported offset.

    Returns a dict with:
      'success'        — bool
      'error'          — str describing why it failed/was skipped (when not successful)
      'phys_offset'    — int physical offset (always present)
      'logical_addr'   — int logical address (always present)
      'failing_path'   — str device path (always present)
      'recovered_data' — bytes (only when successful)
    """
    base = {'phys_offset': phys_offset, 'logical_addr': logical_addr, 'failing_path': failing_path}

    expected_csum = lookup_extent_csum(mount_dev, logical_addr)

    with open(failing_path, 'rb') as f:
        f.seek(phys_offset)
        corrupted_block = f.read(BTRFS_SECTOR_SIZE)

    current_block_csum = crc32c(corrupted_block)
    logger.debug(f"  [0x{phys_offset:x}] on-disk csum: 0x{current_block_csum:08x}  metadata csum: 0x{expected_csum:08x}")

    if current_block_csum == expected_csum:
        return {**base, 'success': False, 'error': "Block matches metadata checksum — no corruption at this location"}
    logger.info(f"  [0x{phys_offset:x}] ✓ Corruption confirmed.")

    # For read-IO failures the kernel also logs the csum it observed. Cross-check
    # that our read matches, to ensure we are looking at the right block.
    if actual_csum_reported is not None:
        current_be = int.from_bytes(current_block_csum.to_bytes(4, 'little'), 'big')
        if current_be != actual_csum_reported:
            return {
                **base,
                'success': False,
                'error': (
                    f"our read csum 0x{current_be:08x} differs from "
                    f"kernel-reported 0x{actual_csum_reported:08x} — wrong location"
                ),
            }
        print(f"  [0x{phys_offset:x}] ✓ Matches kernel-reported csum.")
        logger.debug(f"  [0x{phys_offset:x}] ✓ Matches kernel-reported csum.")

    blocks_to_xor = []
    for path in config.data_devs.values():
        if path == failing_path:
            continue
        with open(path, 'rb') as f:
            f.seek(phys_offset)
            blocks_to_xor.append(f.read(BTRFS_SECTOR_SIZE))
    with open(config.parity_p, 'rb') as f:
        f.seek(phys_offset)
        blocks_to_xor.append(f.read(BTRFS_SECTOR_SIZE))

    # Parity must be inconsistent with the corrupted data for XOR recovery to work.
    # XOR(all data blocks, parity) == 0 means parity was computed from the corrupted
    # byte, so it cannot reconstruct the original.
    parity_check = xor_bytes([corrupted_block] + blocks_to_xor)
    if all(b == 0 for b in parity_check):
        return {
            **base,
            'success': False,
            'error': "Parity is consistent with corrupted data — baked into parity; XOR recovery impossible",
        }

    recovered_data = xor_bytes(blocks_to_xor)
    computed_csum = crc32c(recovered_data)
    print(f"  [0x{phys_offset:x}] expected csum: 0x{expected_csum:08x}  recovered csum: 0x{computed_csum:08x}")

    if computed_csum != expected_csum:
        return {**base, 'success': False, 'error': "Checksum mismatch after XOR recovery"}

    return {**base, 'success': True, 'recovered_data': recovered_data}

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


def handle_failure(
    fields: dict,
    already_recovered: set[int],
) -> list[dict]:
    """Locate and recover all corrupt sectors within the scan window for one log line.

    Each log line provides a single (ino, off) hint that maps to a logical address.
    The ±64-sector scan around that address may reveal multiple corrupt sectors —
    one per distinct physical offset with a CRC32C mismatch.

    already_recovered is a set of physical offsets that have already been written
    back (shared across all log lines to avoid double-recovery when two log lines
    have overlapping scan windows). It is mutated in place.

    Returns a list of result dicts, one per sector attempted.
    """
    try:
        must_be_root()
        failing_dev_name = fields['dev']
        ino, off = int(fields['ino']), int(fields['off'])
        actual_csum_str = fields.get('actual')
        # actual_csum_reported is the checksum btrfs observed for the specific sector
        # named in the log line. It is only valid for the sector at `off`; sectors
        # discovered by scanning nearby are not cross-checked against it.
        actual_csum_reported = int(actual_csum_str, 16) if actual_csum_str else None

        mount_dev = f"/dev/{failing_dev_name}" if not failing_dev_name.startswith('/') else failing_dev_name
        try:
            devid, base_phys, base_logical = find_physical_offset(mount_dev, ino, off)
        except RuntimeError as e:
            # File was deleted (inode no longer has EXTENT_DATA entries) — skip this hint
            if 'No REGULAR EXTENT_DATA found' in str(e):
                print(f"  Inode {ino} no longer found — skipping.")
                return [{'success': False, 'skipped': True,
                         'error': "File was deleted (inode has no EXTENT_DATA entries)",
                         'phys_offset': 0, 'logical_addr': 0,
                         'failing_path': f"/dev/{failing_dev_name}"}]
            raise

        config = get_array_config()
        failing_path = config.data_devs.get(devid)
        if not failing_path:
            return [{'success': False, 'error': f"Could not map DevID {devid} to path",
                     'phys_offset': base_phys, 'logical_addr': base_logical,
                     'failing_path': None}]

        print(f"Failing Device: {failing_path} (DevID: {devid}) | Hint offset: 0x{base_phys:x}")

        corrupt_sectors = find_all_corrupt_sectors(mount_dev, failing_path, base_logical, base_phys)
        if not corrupt_sectors:
            # Check whether the scan window overlaps with sectors we already fixed.
            # If so this is a benign duplicate hint, not a genuine failure.
            SCAN_WINDOW = 64
            window_min = base_phys - SCAN_WINDOW * BTRFS_SECTOR_SIZE
            window_max = base_phys + SCAN_WINDOW * BTRFS_SECTOR_SIZE
            already_covered = any(window_min <= p <= window_max for p in already_recovered)
            if already_covered:
                print(f"  [0x{base_phys:x}] Window already covered by a previous log line — recovered by proxy.")
                return [{'success': True, 'skipped': False,
                         'phys_offset': base_phys, 'logical_addr': base_logical,
                         'failing_path': failing_path}]
            return [{'success': False, 'skipped': False,
                     'error': "No mismatching sectors found in ±64-sector window",
                     'phys_offset': base_phys, 'logical_addr': base_logical,
                     'failing_path': failing_path}]

        print(f"  Found {len(corrupt_sectors)} corrupt sector(s) in window.")

        results = []
        for logical, phys in corrupt_sectors:
            if phys in already_recovered:
                print(f"  [0x{phys:x}] Already recovered by a previous log line — recovered by proxy.")
                results.append({'success': True, 'skipped': False,
                                'phys_offset': phys, 'logical_addr': logical,
                                'failing_path': failing_path})
                continue

            # Only pass actual_csum_reported for the sector the log line named.
            reported_csum = actual_csum_reported if logical == base_logical else None

            result = recover_sector(mount_dev, config, failing_path, logical, phys, reported_csum)
            result.setdefault('skipped', False)
            results.append(result)

            if result['success']:
                write_recovered_data(result['recovered_data'], phys, failing_path)
                already_recovered.add(phys)
            else:
                print(f"  [0x{phys:x}] ✗ {result['error']}")

        return results

    except Exception as e:
        return [{'success': False, 'skipped': False, 'error': f"Exception: {e}",
                 'phys_offset': 0, 'logical_addr': 0, 'failing_path': None}]


def monitor_dmesg(log_file: str | None = None):
    """
    Reads the current dmesg buffer (or a log file), processes all BTRFS csum
    failures, and attempts to recover each corrupt sector. Sectors already
    recovered from a prior log line's window are skipped.
    """
    if log_file:
        print(f"Scanning log file '{log_file}' for BTRFS csum failures...")
        try:
            with open(log_file, 'r') as f:
                log_data = f.read()
        except OSError as e:
            sys.exit(f"ERROR: Failed to read log file: {e}")
    else:
        print("Scanning dmesg for BTRFS csum failures...")
        try:
            log_data = subprocess.check_output(['dmesg'], text=True)
        except subprocess.CalledProcessError as e:
            sys.exit(f"ERROR: Failed to read dmesg: {e}")

    seen_hints: set[tuple] = set()
    hints: list[dict] = []
    for line in log_data.splitlines():
        if 'BTRFS warning' not in line:
            continue
        fields = _extract_fields(line)
        if fields is None:
            continue
        key = (fields['dev'], fields['ino'], fields['off'])
        if key in seen_hints:
            continue
        seen_hints.add(key)
        hints.append(fields)

    if not hints:
        print("No BTRFS checksum failures found in dmesg.")
        return

    print(f"\n[*] Found {len(hints)} unique log hints. Scanning recovery windows...\n")

    already_recovered: set[int] = set()
    all_results: list[dict] = []

    for fields in hints:
        print(f"\n{'='*70}")
        print(f"[!] {fields['type']} — ino={fields['ino']} off={fields['off']} dev={fields['dev']}")
        print(f"{'='*70}")
        results = handle_failure(fields, already_recovered)
        all_results.extend(results)

    # Summary
    successful = [r for r in all_results if r['success']]
    skipped    = [r for r in all_results if not r['success'] and r.get('skipped')]
    failed     = [r for r in all_results if not r['success'] and not r.get('skipped')]
    print(f"\n{'='*70}")
    print(f"[*] RECOVERY SUMMARY")
    print(f"{'='*70}")
    print(f"Log line hints processed : {len(hints)}")
    print(f"Sectors attempted        : {len(all_results)}")
    print(f"  ✓ Recovered            : {len(successful)}")
    print(f"  ↷ Skipped              : {len(skipped)}")
    print(f"  ✗ Failed               : {len(failed)}")
    if failed:
        print("\nFailed sectors:")
        for r in failed:
            print(f"  phys=0x{r['phys_offset']:x} — {r['error']}")


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='Recover BTRFS corrupted blocks using parity.')
    parser.add_argument(
        '--log-file', metavar='FILE',
        help='Path to a kernel log file to scan instead of running dmesg.'
    )
    args = parser.parse_args()
    monitor_dmesg(log_file=args.log_file)