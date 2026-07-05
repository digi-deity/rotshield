#!/usr/bin/env python3
"""Diagnostic: prove that reading an array-physical address P from the array
partition (/dev/nmdNp1) and reading P + rdevOffset*512 from the raw backing
device (/dev/loopN) return identical bytes."""

def rdev_offset(slot: int) -> int:
    key = f'rdevOffset.{slot}'
    with open('/proc/nmdstat') as f:
        for line in f:
            if line.strip().startswith(key + '='):
                return int(line.strip().split('=', 1)[1]) * 512
    return 0

OFF1 = rdev_offset(1)   # slot 1 → /dev/loop2
print(f'rdevOffset.1 = {OFF1} bytes (0x{OFF1:x})')

P = 0x10000  # arbitrary array-physical offset where real array data lives

with open('/dev/nmd1p1', 'rb') as fp:
    fp.seek(P)
    via_partition = fp.read(4096)

with open('/dev/loop2', 'rb') as fp:
    fp.seek(P + OFF1)
    via_raw = fp.read(4096)

print(f'partition[0x{P:x}] == raw[0x{P + OFF1:x}]: {via_partition == via_raw}')
# also show what we'd get if we (wrongly) read raw at P with no offset
with open('/dev/loop2', 'rb') as fp:
    fp.seek(P)
    via_raw_nooff = fp.read(4096)
print(f'partition[0x{P:x}] == raw[0x{P:x}] (no offset, WRONG): {via_partition == via_raw_nooff}')