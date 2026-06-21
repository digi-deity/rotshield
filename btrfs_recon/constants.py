BTRFS_MAGIC: bytes = b"_BHRfS_M"
BTRFS_UUID_SIZE: int = 16
BTRFS_LABEL_SIZE: int = 256
BTRFS_CSUM_SIZE: int = 32
BTRFS_FSID_SIZE: int = 16

# Size of a data sector in bytes — the granularity at which btrfs stores checksums.
BTRFS_SECTOR_SIZE: int = 4096
