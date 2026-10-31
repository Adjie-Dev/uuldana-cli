use std::{fmt, io};
use uuldana_core::{LaunchPlanError, TerminalSize};

/// Failures at the process boundary, separate from provider error classification.
#[derive(Debug)]
pub enum Error {
    /// Structural validation failed before spawning.
    InvalidPlan(LaunchPlanError),
    /// Operation intentionally unavailable in this prototype.
    Unsupported(&'static str),
    /// Dimensions cannot be represented by the native backend.
    InvalidSize(TerminalSize),
    /// Native operating system failure.
    Io(io::Error),
    /// PTY backend failure.
    Backend(String),
    /// A line exceeded the one MiB record limit.
    RecordTooLarge,
    /// A structured record was not UTF-8.
    InvalidEncoding,
    /// A worker terminated without replying.
    WorkerStopped,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPlan(e) => e.fmt(f),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::InvalidSize(s) => {
                write!(f, "unrepresentable terminal size {}x{}", s.rows, s.cols)
            }
            Self::Io(e) => e.fmt(f),
            Self::Backend(s) => write!(f, "PTY backend: {s}"),
            Self::RecordTooLarge => f.write_str("structured record exceeds 1 MiB"),
            Self::InvalidEncoding => f.write_str("structured record is not UTF-8"),
            Self::WorkerStopped => f.write_str("transport worker stopped"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::InvalidPlan(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
