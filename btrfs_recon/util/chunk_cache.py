from __future__ import annotations

from typing import Iterable, TYPE_CHECKING

import construct as cs
from intervaltree import Interval, IntervalTree

from btrfs_recon.types import DevId, PhysicalAddress

if TYPE_CHECKING:
    from btrfs_recon import structure


# BlockGroupFlag bits that mark *mirrored* profiles (every stripe holds a
# full copy of the chunk's data, so any one can be read independently).
_MIRROR_FLAGS = (
    1 << 4   # RAID1
    | 1 << 5 # DUP
    | 1 << 9 # RAID1C3
    | 1 << 10  # RAID1C4
)


class ChunkTreeCache(IntervalTree):
    def insert(
        self,
        log_start: int,
        log_end: int,
        stripes: (
            Iterable[tuple[DevId, PhysicalAddress]]
            | dict[DevId, PhysicalAddress]
            | Iterable[cs.Container | structure.Stripe]
        ),
        stripe_len: int | None = None,
        flags: int = 0,
    ) -> Interval:
        """Record a mapping of logical -> physical for a block of logical address space.

        `stripe_len` defaults to the chunk length (correct for single-stripe
        and DUP/mirrored chunks). `flags` is the chunk's BlockGroupFlag value
        (used to distinguish mirrored profiles like DUP/RAID1 from striped
        ones like RAID0/RAID10).
        """
        from btrfs_recon import structure

        if stripe_len is None:
            stripe_len = log_end - log_start

        if not isinstance(stripes, dict):
            stripes = tuple(stripes)
            assert stripes

            if isinstance(stripes[0], (cs.Container, structure.Stripe)):
                stripes = [(stripe.devid, stripe.offset) for stripe in stripes]

        mirrored = bool(flags & _MIRROR_FLAGS)

        if matches := self[log_start:log_end]:
            assert len(matches) == 1
            ival, = matches
            ival.data['stripe_len'] = stripe_len
            ival.data['stripes'] = stripes
            ival.data['mirrored'] = mirrored
        else:
            ival = Interval(log_start, log_end, {
                'stripe_len': stripe_len,
                'stripes': stripes,
                'mirrored': mirrored,
            })
            self.add(ival)

        return ival

    def offsets(self, logical: int, size: int = 1) -> Iterable[tuple[DevId, PhysicalAddress, int]]:
        """Return the mapped physical addresses for the given logical address.

        For *mirrored* profiles (DUP, RAID1, RAID1C3, RAID1C4), every stripe
        holds a complete copy of the data, so we yield each stripe's address
        for the offset (callers pick one — typically the first).

        For *striped* profiles (RAID0/RAID10/RAID5/RAID6) and the common
        single-stripe case, the original stripe-unit math applies.
        """
        blocks: set[Interval] = self.at(logical)
        assert len(blocks) <= 1, \
            f'Multiple logical blocks matched {logical}. This should never happen.'

        if not blocks:
            raise KeyError(f'Unable to find physical address mapping for logical address {logical}')

        block = next(iter(blocks))

        stripe_len = block.data['stripe_len']
        stripes = block.data['stripes']
        num_stripes = len(stripes)
        mirrored = block.data.get('mirrored', False)

        log_offset = logical - block.begin

        if mirrored or num_stripes == 1:
            # Mirrored: each stripe is a full copy → yield every copy.
            # Single-stripe: identical to mirroring with one copy.
            remaining = size
            for devid, chunk_phys in stripes:
                # Each copy spans the whole chunk, so the in-chunk offset
                # is just `log_offset` (no striping).
                phys = chunk_phys + log_offset
                yield devid, phys, remaining
            return

        # Striped: original stripe-unit walking logic.
        pre_stripe_units = log_offset // stripe_len
        stripe_offset = log_offset % stripe_len

        while size > 0:
            n_stripe_units = pre_stripe_units // num_stripes
            stripe_idx = pre_stripe_units % num_stripes
            (devid, chunk_phys) = stripes[stripe_idx]

            num_bytes = min(size, stripe_len, stripe_len - stripe_offset)
            phys = chunk_phys + n_stripe_units * stripe_len + stripe_offset
            yield devid, phys, num_bytes

            pre_stripe_units += 1
            stripe_offset = 0
            size -= num_bytes

    def reverse_trees(self) -> dict[DevId, IntervalTree]:
        """Return a tree mapping physical -> logical for each device in the cache"""
        rtrees: dict[DevId, IntervalTree] = {}

        for ival in self.all_intervals:
            for devid, physical in ival.data['stripes']:
                if devid not in rtrees:
                    rtrees[devid] = IntervalTree()
                rtrees[devid].addi(physical, physical + ival.length(), ival.begin)

        return rtrees
