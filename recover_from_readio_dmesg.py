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
This script monitors the dmesg buffer for BTRFS checksum failures after a read I/O operation, extracts the relevant information,
and attempts to recover the corrupted data using parity information.

Works on this type of dmesg message:
[218470.835135] BTRFS warning (device nmd1p1): csum failed root 5 ino 257 off 167936 csum 0x28f86fd2 expected csum 0xf8d99c3c mirror 1
[218470.835140] BTRFS error (device nmd1p1): bdev /dev/nmd1p1 errs: wr 0, rd 0, flush 0, corrupt 7, gen 0
"""

from __future__ import annotations
import os
import re
import sys
import struct
import subprocess
import time
from crc32c import crc32c
from btrfs_recon.parsing import parse_superblock, build_chunk_tree, walk_btree
from btrfs_recon.structure import KeyType, ObjectId

CSUM_BLOCK_SIZE = 4096
DEDUP_WINDOW_SECONDS = 5 

def must_be_root() -> None:
    if os.geteuid() != 0:
        sys.exit("Please run as root (sudo).")

def get_array_config() -> dict:
    """Parses /proc/nmdstat and strictly validates device paths."""
    if not os.path.exists('/proc/nmdstat'):
        sys.exit("ERROR: /proc/nmdstat not found.")
    
    values = {}
    with open('/proc/nmdstat', 'r') as f:
        for line in f:
            line = line.strip()
            if "=" in line:
                key, _, value = line.partition("=")
                values[key] = value

    config = {'data_devs': {}, 'parity_p': None, 'parity_q': None}
    
    for key, value in values.items():
        # Only process keys that are exactly 'rdevName.<number>'
        if key.startswith('rdevName.'):
            try:
                slot_str = key.split('.')[1]
                slot = int(slot_str)
            except (ValueError, IndexError):
                continue
            
            if not value or value.strip() == "":
                continue

            # Normalize path
            full_path = value if value.startswith('/') else f'/dev/{value}'
            
            # CRITICAL FIX: Ensure the path is actually a block device and not a directory
            if not os.path.exists(full_path) or os.path.isdir(full_path):
                continue

            if slot == 0:
                config['parity_p'] = full_path
            elif slot == 29:
                config['parity_q'] = full_path
            else:
                config['data_devs'][slot] = full_path

    if not config['parity_p']:
        sys.exit("ERROR: Primary parity disk (Slot 0) not found or invalid in /proc/nmdstat")
        
    return config

def find_physical_offset(mount_dev: str, target_ino: int, target_off: int):
    with open(mount_dev, 'rb') as fp:
        superblock = parse_superblock(fp)
        devid_fp_map = {superblock.dev_item.devid: fp}
        tree = build_chunk_tree(superblock, devid_fp_map)

        fs_root_bytenr = None
        for item in walk_btree(superblock.root, tree, devid_fp_map):
            if item.key.objectid == ObjectId.FsTree and item.key.ty == KeyType.RootItem:
                fs_root_bytenr = item.data.bytenr
                break
        if fs_root_bytenr is None: raise RuntimeError("FS_TREE ROOT_ITEM not found")
        
        found_item = None
        for item in walk_btree(fs_root_bytenr, tree, devid_fp_map):
            if item.key.objectid == target_ino and item.key.ty == KeyType.ExtentData:
                if item.key.offset <= target_off < item.key.offset + item.data.ref.disk_num_bytes:
                    found_item = item
                    break
        if not found_item: raise RuntimeError(f"Could not find extent for ino {target_ino}")

        logical_addr = found_item.data.ref.disk_bytenr + (target_off - found_item.key.offset)
        chunks = list(tree.at(logical_addr))
        if not chunks:
            raise RuntimeError(f"No chunk mapping for logical address {logical_addr}")
        
        chunk = chunks[0]
        phys_base = chunk.data['stripes'][0][1]
        abs_phys_offset = phys_base + (logical_addr - chunk.begin)
        devid = chunk.data['stripes'][0][0]
        
        return devid, abs_phys_offset

def xor_bytes(data_list: list[bytes]) -> bytes:
    result = bytearray(data_list[0])
    for next_block in data_list[1:]:
        for i in range(len(result)):
            result[i] ^= next_block[i]
    return bytes(result)

def monitor_dmesg():
    """
    Reads the current dmesg buffer, processes all BTRFS csum failures,
    and then exits.
    """
    print(f"Scanning dmesg for BTRFS csum failures...")
    
    # Use check_output to get the current buffer as a string and return immediately
    try:
        log_data = subprocess.check_output(['dmesg'], text=True)
    except subprocess.CalledProcessError as e:
        sys.exit(f"ERROR: Failed to read dmesg: {e}")

    pattern = re.compile(
        r'BTRFS warning \(device (?P<dev>\S+)\): csum failed root (?P<root>\d+) '
        r'ino (?P<ino>\d+) off (?P<off>\d+) csum 0x(?P<actual>[0-9a-f]+) '
        r'expected csum 0x(?P<expected>[0-9a-f]+)'
    )

    # Find all matches in the current buffer
    matches = pattern.finditer(log_data)
    
    found_any = False
    seen_errors = set()

    for match in matches:
        # Deduplicate based on device, inode, and offset
        error_key = (match.group('dev'), match.group('ino'), match.group('off'))
        if error_key in seen_errors:
            continue
        
        seen_errors.add(error_key)
        found_any = True
        
        print(f"\n[!] Detected Corruption: {match.group(0)}")
        try:
            handle_failure(match)
        except Exception as e:
            print(f"Skipping recovery: {e}")

    if not found_any:
        print("No BTRFS checksum failures found in dmesg.")
    else:
        print(f"\nFinished processing {len(seen_errors)} unique errors.")

def handle_failure(match):
    must_be_root()
    failing_dev_name = match.group('dev')
    ino, off = int(match.group('ino')), int(match.group('off'))
    actual_csum_reported = int(match.group('actual'), 16)
    expected_csum = int(match.group('expected'), 16)
    
    mount_dev = f"/dev/{failing_dev_name}" if not failing_dev_name.startswith('/') else failing_dev_name
    devid, phys_offset = find_physical_offset(mount_dev, ino, off)
    
    config = get_array_config()
    failing_path = config['data_devs'].get(devid)
    if not failing_path: raise RuntimeError(f"Could not map DevID {devid} to path")

    print(f"Failing Device: {failing_path} (DevID: {devid}) | Offset: 0x{phys_offset:x}")

    with open(failing_path, 'rb') as f:
        f.seek(phys_offset)
        corrupted_block = f.read(CSUM_BLOCK_SIZE)
    
    current_block_csum = crc32c(corrupted_block)
    if current_block_csum != actual_csum_reported:
        swapped = struct.unpack('<I', struct.pack('>I', current_block_csum))[0]
        if swapped == actual_csum_reported:
            current_block_csum = swapped

    print(f"Reported Actual Csum: 0x{actual_csum_reported:08x}")
    print(f"Read Block Csum:      0x{current_block_csum:08x}")
    
    if current_block_csum != actual_csum_reported:
        raise RuntimeError(f"Verification failed! Block at 0x{phys_offset:x} does not match dmesg.")
    
    print("✓ Verification successful.")

    blocks_to_xor = []
    for slot, path in config['data_devs'].items():
        if path == failing_path: continue 
        with open(path, 'rb') as f:
            f.seek(phys_offset)
            blocks_to_xor.append(f.read(CSUM_BLOCK_SIZE))
            
    with open(config['parity_p'], 'rb') as f:
        f.seek(phys_offset)
        blocks_to_xor.append(f.read(CSUM_BLOCK_SIZE))

    recovered_data = xor_bytes(blocks_to_xor)
    computed_csum = crc32c(recovered_data)
    
    if computed_csum != expected_csum:
        swapped = struct.unpack('<I', struct.pack('>I', computed_csum))[0]
        if swapped == expected_csum: computed_csum = swapped

    print(f"Expected Csum: 0x{expected_csum:08x} | Recovered: 0x{computed_csum:08x}")
    if computed_csum == expected_csum:
        print("✓ SUCCESS: Data recovered!")
    else:
        print("✗ FAILURE: Checksum still mismatch.")

if __name__ == '__main__':
    monitor_dmesg()
