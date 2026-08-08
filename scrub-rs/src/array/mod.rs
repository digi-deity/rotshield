//! The NonRAID array layer: config parsing, stripe I/O, and parity math.

//!
//! * `config` — /proc/nmdstat parsing and slot/rdev lookups.
//! * `stripe` — per-offset chunk reads/writes across the array.
//! * `parity` — P/Q syndrome computation.

pub mod config;
pub mod parity;
pub mod stripe;
