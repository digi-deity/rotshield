#!/usr/bin/env python3

# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "crc32c>=2.2.post0",
#   "btrfs-recon",
# ]
# ///
#!/usr/bin/env python3
#!/usr/bin/env python/3
#!/usr/bin/env python3
"""
RAID Recovery Script - DEVID RESOLUTION FIX
Correctes Btrfs checksum errors using RAID-P parity.
Uses Btrfs DevID to resolve the failing disk, ignoring inconsistent naming.
"""

import os
import re
import sys
import struct
import subprocess
from crc32c import crc32c
from btrfs_recon.parsing import parse_superblock, build_chunk_tree, walk_btree, parse_at
from btrfs_recon.structure import KeyType, ObjectId, TreeNode, Header

CSUM_BLOCK_SIZE = 4096
CSUM_SIZE_CRC32C = 4

def must_be_root() -> None:
    if os.geteuid() != 0:
        sys.exit("Please run as root (sudo).")

def normalize_path(path: str) -> str:
    path = path.strip()
    if path.startswith('/'): return path
    return f'/dev/{path}'

def _is_block_device(path: str) -> bool:
    try:
        return os.path.exists(path) and (os.stat(path).st_mode & 0o170000) == 0o060000
    except OSError:
        return False

def get_nmdstat_mapping() -> dict[int, str]:
    mapping = {}
    if not os.path.exists('/proc/nmdstat'):
        sys.exit("ERROR: /proc/nmdstat not found.")
    with open('/proc/nmdstat', 'r') as f:
        for line in f:
            line = line.strip()
            if line.startswith('rdevName.'):
                try:
                    parts = line.split('=')
                    slot = int(line.split('.')[1].split('=')[0])
                    name = parts[1].strip()
                    full_path = normalize_path(name)
                    if _is_block_device(full_path):
                        mapping[slot] = full_path
                except (IndexError, ValueError):
                    continue
    return mapping

def find_stored_csum(fp, tree, superblock, devid_fp_map, logical_addr):
    csum_root = None
    for item in walk_btree(superblock.root, tree, devid_fp_map):
        if item.key.objectid == ObjectId.CsumTree and item.key.ty == KeyType.RootItem:
            csum_root = item.data.bytenr
            break
    if csum_root is None: return None
    from collections import deque
    root_matches = list(tree.offsets(csum_root))
    queue = deque([(d, p) for d, p, _ in root_matches])
    while queue:
        devid, phys = queue.popleft()
        if devid not in devid_fp_map: continue
        current_fp = devid_fp_map[devid]
        node = parse_at(current_fp, phys, TreeNode)
        if node.header.level == 0:
            for item in node.items:
                if item.key.ty == KeyType.ExtentCsum:
                    covered_len = (item.size // CSUM_SIZE_CRC32C) * CSUM_BLOCK_SIZE
                    if item.key.offset <= logical_addr < item.key.offset + covered_len:
                        block_idx = (logical_addr - item.key.offset) // CSUM_BLOCK_SIZE
                        csum_pos = (node.phys_start + Header.sizeof() + 
                                    item.offset + (block_idx * CSUM_SIZE_CRC32C))
                        current_fp.seek(csum_pos)
                        return struct.unpack('<I', current_fp.read(CSUM_SIZE_CRC32C))[0]
        else:
            for ptr in node.items:
                for d, p, _ in tree.offsets(ptr.blockptr):
                    queue.append((d, p))
    return None

def xor_bytes(data_list: list[bytes]) -> bytes:
    if not data_list: return b''
    result = bytearray(data_list[0])
    for next_block in data_list[1:]:
        for i in range(len(result)):
            result[i] ^= next_block[i]
    return bytes(result)

def recover_scrub_error():
    must_be_root()
    nmd_map = get_nmdstat_mapping()
    
    print("Scanning dmesg for scrub checksum failures...")
    log_data = subprocess.check_output(['dmesg'], text=True)
    
    pattern = re.compile(
        r'(?P<full_line>BTRFS warning \(device (?P<dev>\S+)\): checksum error at logical (?P<log>\d+) '
        r'on dev (?P<dev_path>\S+), physical (?P<phys>\d+), root (?P<root>\d+), '
        r'inode (?P<ino>\d+), offset (?P<off>\d+), length (?P<len>\d+))'
    )

    for match in pattern.finditer(log_data):
        full_line = match.group('full_line')
        logical = int(match.group('log'))
        phys_base = int(match.group('phys'))
        dmesg_path = normalize_path(match.group('dev_path'))

        print(f"\n{'='*60}")
        print(f"DMESG LINE: {full_line}")
        print(f"TARGET: Logical {logical} | Physical {phys_base}")
        print(f"{'='*60}")

        # 1. Resolve the ACTUAL device path using Btrfs DevID
        # We open the dmesg path to read the superblock and chunk tree
        with open(dmesg_path, 'rb') as fp:
            sb = parse_superblock(fp)
            # Map the Btrfs DevID to the file pointer
            devid_fp_map = {sb.dev_item.devid: fp}
            tree = build_chunk_tree(sb, devid_fp_map)
            
            # Find which Btrfs DevID is actually associated with this logical address
            chunks = list(tree.at(logical))
            if not chunks:
                print("Could not find chunk for logical address. Skipping.")
                continue
            
            # The first stripe in the chunk tells us the Btrfs DevID of the failing block
            btrfs_devid = chunks[0].data['stripes'][0][0]
            
            # Now we find which physical device in our nmd_map matches this Btrfs DevID
            # We do this by checking the superblocks of all disks in nmd_map
            resolved_corrupt_path = None
            for slot, path in nmd_map.items():
                try:
                    with open(path, 'rb') as check_fp:
                        check_sb = parse_superblock(check_fp)
                        if check_sb.dev_item.devid == btrfs_devid:
                            resolved_corrupt_path = path
                            break
                except Exception:
                    continue
            
            if not resolved_corrupt_path:
                print(f"Could not resolve Btrfs DevID {btrfs_devid} to any nmdstat disk. Skipping.")
                continue

            print(f"RESOLVED CORRUPT DISK: {resolved_corrupt_path} (Btrfs DevID: {btrfs_devid})")
            
            expected_csum = find_stored_csum(fp, tree, sb, devid_fp_map, logical)

        if expected_csum is None:
            print("Could not find stored checksum. Skipping.")
            continue

        # Verify corruption on the resolved path
        found_sector_phys = None
        with open(resolved_corrupt_path, 'rb') as f:
            for i in range(16):
                current_phys = phys_base + (i * CSUM_BLOCK_SIZE)
                f.seek(current_phys)
                block = f.read(CSUM_BLOCK_SIZE)
                if crc32c(block) != expected_csum:
                    found_sector_phys = current_phys
                    print(f"✓ Confirmed corrupt sector at 0x{found_sector_phys:x}")
                    break
        
        if found_sector_phys is None:
            print("Could not locate corrupt sector. Skipping.")
            continue

        # XOR RECOVERY PHASE
        parity_p_block = None
        parity_p_path = None
        healthy_data_blocks = []
        data_disk_labels = []
        max_slot = max(nmd_map.keys())

        for slot, path in nmd_map.items():
            # 1. Handle Parity P (Slot 0)
            if slot == 0:
                if path == resolved_corrupt_path:
                    print("CRITICAL: Parity P is the corrupt disk. XOR recovery impossible.")
                    continue
                with open(path, 'rb') as f:
                    f.seek(found_sector_phys)
                    parity_p_block = f.read(CSUM_BLOCK_SIZE)
                    parity_p_path = path
                continue

            # 2. Handle Q-Parity (Hardcoded Slot 29)
            if slot == 29:
                print(f"Ignoring Q-Parity Disk (Slot 29: {path}) - Not used in XOR recovery.")
                continue

            # 3. Handle Data Disks - STRICT EXCLUSION based on resolved path
            if path == resolved_corrupt_path:
                print(f"EXCLUDING corrupt disk from XOR: {path} (Slot {slot})")
                continue
            
            with open(path, 'rb') as f:
                f.seek(found_sector_phys)
                healthy_data_blocks.append(f.read(CSUM_BLOCK_SIZE))
                data_disk_labels.append(f"Disk {slot} ({path})")

        if parity_p_block is None:
            print("ERROR: Parity P block missing. Recovery aborted.")
            continue

        print("\n--- XOR Calculation Plan ---")
        xor_plan = [f"Parity P ({parity_p_path})"] + data_disk_labels
        print(f"Equation: {' XOR '.join(xor_plan)}")
        print(f"Total blocks in XOR: {len(xor_plan)}")
        print("----------------------------\n")

        recovered_data = xor_bytes([parity_p_block] + healthy_data_blocks)
        recovered_csum = crc32c(recovered_data)

        if recovered_csum == expected_csum:
            print(f"✓ SUCCESS: Recovered Csum 0x{recovered_csum:08x} matches Expected!")
            print(f"DRY RUN: Write to {resolved_corrupt_path} at 0x{found_sector_phys:x}")
        else:
            print(f"✗ FAILURE: Recovered 0x{recovered_csum:08x} != Expected 0x{expected_csum:08x}")

if __name__ == '__main__':
    recover_scrub_error()
