#!/usr/bin/env python3
"""Determine whether writing the recovery through the array partition
(/dev/nmd1p1) versus the raw rdev (/dev/loop2) makes the corrected byte
visible to subsequent reads through the array device.

Sequence (on the currently-corrupt sector at array_phys=0x6564000):
 1. Show current byte on /dev/nmd1p1 @ 0x6564000 (corrupt) and /dev/loop2 @@
    0x656c000 (recovered).
 2. Drop all caches.
 3. Re-read both → does the array device now see the corrected byte?

If the array device STILL shows the corrupt byte after drop_caches, the nmd
driver has an untouched cache and writing through the raw rdev is invisible
to btrfs.  We then have to consider whether recovery should write through
the array partition instead.
"""
import os
import subprocess

ARRAY = '/dev/nmd1p1'
RAW   = '/dev/loop2'
ARRAY_OFF = 0x6564000
RAW_OFF   = 0x656c000

def rb(path, off):
    with open(path, 'rb') as f:
        f.seek(off); return f.read(1)[0]

def wb(path, off, b):
    with open(path, 'r+b') as f:
        f.seek(off); f.write(bytes([b])); f.flush(); os.fsync(f.fileno())

def drop():
    os.sync()
    with open('/proc/sys/vm/drop_caches', 'w') as f:
        f.write('3')
    for d in (ARRAY, RAW, '/dev/loop0'):
        subprocess.run(['blockdev', '--flushbufs', d],
                       stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL)

print('Step 1: current bytes (after recovery wrote 0x64 to raw)')
print(f'  {ARRAY} @ 0x{ARRAY_OFF:x} = 0x{rb(ARRAY,ARRAY_OFF):02x} (expect 0x9b corrupt)')
print(f'  {RAW}   @ 0x{RAW_OFF:x} = 0x{rb(RAW,RAW_OFF):02x} (expect 0x64 recovered)')

print('\nStep 2: drop all caches + flushbufs, re-read')
drop()
print(f'  {ARRAY} @ 0x{ARRAY_OFF:x} = 0x{rb(ARRAY,ARRAY_OFF):02x}')
print(f'  {RAW}   @ 0x{RAW_OFF:x} = 0x{rb(RAW,RAW_OFF):02x}')

print('\nStep 3: write 0x64 directly through ARRAY partition at 0x6564000')
wb(ARRAY, ARRAY_OFF, 0x64)
print(f'  {ARRAY} @ 0x{ARRAY_OFF:x} = 0x{rb(ARRAY,ARRAY_OFF):02x}')

print('\nStep 4: drop caches, re-read')
drop()
print(f'  {ARRAY} @ 0x{ARRAY_OFF:x} = 0x{rb(ARRAY,ARRAY_OFF):02x}')
print(f'  {RAW}   @ 0x{RAW_OFF:x} = 0x{rb(RAW,RAW_OFF):02x}')

print('\nStep 5: now corrupt it again on raw rdev to restore test state')
wb(RAW, RAW_OFF, 0x9b)
drop()
print(f'  {ARRAY} @ 0x{ARRAY_OFF:x} = 0x{rb(ARRAY,ARRAY_OFF):02x}')
print(f'  {RAW}   @ 0x{RAW_OFF:x} = 0x{rb(RAW,RAW_OFF):02x}')

print('\nStep 6: read via btrfs (mount path) to confirm what kernel sees')
import subprocess
subprocess.run(['dmesg', '-C'])
subprocess.run(['dd', 'if=/mnt/disk1/file1', 'of=/dev/null', 'bs=4K',
                'count=200'], stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
out = subprocess.run(['dmesg'], capture_output=True, text=True).stdout
warnings = [l for l in out.splitlines() if 'BTRFS warning' in l]
print(f'  dmesg BTRFS warnings after read: {len(warnings)}')
for l in warnings[:2]:
    print('  ', l)