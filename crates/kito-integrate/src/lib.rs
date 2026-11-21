//! `kito-integrate`: the integration queue and the verifier.
//!
//! Lanes ask to be integrated into the main lane; the requests are served
//! one at a time, in order, because each one changes what the next is
//! merged with. Before a merged result becomes the main lane, the commands
//! the repository configures (`[verify] commands`) must succeed on it: a
//! merge that composes cleanly can still be broken.
//!
//! This crate holds what does not depend on the repository itself: the
//! queue, the verifier and the result an agent gets back. The daemon wires
//! them to the merge and to the log.

mod queue;
mod result;
mod verify;

pub use queue::{JobId, JobState, Queue};
pub use result::{AutoResolution, ConflictInfo, IntegrationResult, Outcome, VerificationReport};
pub use verify::{run_program, verify, Confinement, Ran};
