# btrfs-recon: a minimal inspection-only btrfs on-disk structure library.
#
# Parsers for btrfs structures (superblock, tree nodes, chunk items, etc.)
# live under `btrfs_recon.structure`. The `btrfs_recon.parsing` module
# provides helpers for walking a btrfs filesystem image in place
# (without any DB or write-back functionality).
