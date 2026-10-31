//! PTY and custom CLI transports. No provider semantics or agent loop lives here.
//!
//! Windows pipe and native PTY children start suspended and join a kill-on-close
//! Job before execution. The vendored portable-pty patch retains the primary
//! thread handle and exposes only a contained resume operation; see its
//! UULDANA_PATCH.md for provenance, safety boundaries and crash-window limits.
//! Unix children belong to a dedicated process group or
//! PTY session; descendants that deliberately leave that group can escape it.
//! Group cleanup occurs before reaping the leader to avoid PID reuse hazards.
//!
//! Direct-child exit and stream EOF are independent. Waiting for new output is
//! bounded to 250 ms after reaping; queued frames survive that deadline. A
//! reader that is still alive at that point gets one final 500 ms window,
//! granted once and never replenished. An exhausted bound emits
//! [`Frame::Diagnostic`] before the single final exit frame, and
//! [`Process::output_verified`] then reports false: output produced before exit
//! may not have been delivered. Emergency drop errors are reported on stderr
//! because a dropped owner has no remaining event receiver.
mod environment;
mod error;
mod process;

pub use error::Error;
pub use process::{Frame, InputHandle, Process};
