//! Parity recovery: GF(2^8) math, the recovery cascade, and the result model.

pub mod engine;
pub mod gf;
pub mod model;

pub use engine::{recover_block, recover_via_p, recover_via_q, solve_two_disk};
pub use model::{FailureReason, ParityPath, RecoveryInput, RecoveryResult};
