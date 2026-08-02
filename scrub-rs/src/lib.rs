//! scrub-rs library crate — shared modules for the scrub binary and
//! utility binaries (corruption crafting, etc.).
//!
//! Design intent: keep the filesystem (btrfs), array, and parity-recovery
//! concerns in separate modules so each can be understood and tested on
//! its own.
//!
//! * `btrfs/` — the scrub walker; implements the contract in `fs/`.
//! * `array/` — loads + aligns chunks across array disks; also computes
//!   live P/Q syndromes for the corruption-crafting tool (via the shared
//!   GF tables in `recovery::gf`).
//! * `recovery/` — pure parity math.
//! * `fs/` — the filesystem-scrub contract (`FilesystemScrub`,
//!   `ScrubCallbacks`, `ScrubEvent`, `Reconfirmer`).
//! * `batch_recover.rs` — the batched two-stage recovery pipeline.
//! * `canary.rs` — the startup array-soundness probe.
//! * `freeze.rs` — live-mount freeze control for recovery writes.
//! * `status.rs` — opt-in HTTP status server + shared live counters so the
//!   unRAID plugin can show a running scrub's error/progress numbers without
//!   polling process logs.  Std-only; the counters mirror what the scrub and
//!   recovery writer already accumulate.

pub mod array;
pub mod batch_recover;
pub mod btrfs;
pub mod canary;
pub mod freeze;
pub mod fs;
pub mod recovery;
pub mod status;
