use std::fmt;
use uuldana_core::*;

/// Typed failures from supervision and policy validation.
#[derive(Debug)]
pub enum Error {
    /// Adapter detection, launch planning, or unsupported capability.
    Adapter(AdapterError),
    /// Policy cannot be represented without escalation.
    Permission(PermissionError),
    /// Session lifecycle invariant failed.
    Session(SessionError),
    /// Launch plan is structurally invalid.
    Plan(LaunchPlanError),
    /// Failure at the process boundary.
    Transport(uuldana_transport::Error),
    /// Provider id was not registered.
    UnknownProvider(ProviderId),
    /// A background task failed.
    WorkerStopped,
    /// Process teardown exceeded its bounded drain time.
    ShutdownTimeout,
    /// Transport ended without delivering its authoritative final exit frame.
    TransportExhausted,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(e) => e.fmt(f),
            Self::Permission(e) => e.fmt(f),
            Self::Session(e) => e.fmt(f),
            Self::Plan(e) => e.fmt(f),
            Self::Transport(e) => e.fmt(f),
            Self::UnknownProvider(id) => write!(f, "unregistered provider: {id}"),
            Self::WorkerStopped => f.write_str("runtime worker stopped"),
            Self::ShutdownTimeout => f.write_str("process shutdown or output drain timed out"),
            Self::TransportExhausted => f.write_str("transport exhausted before run finalization"),
        }
    }
}
impl std::error::Error for Error {}
macro_rules! convert {
    ($t:ty, $v:ident) => {
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Self::$v(e)
            }
        }
    };
}
convert!(AdapterError, Adapter);
convert!(PermissionError, Permission);
convert!(SessionError, Session);
convert!(LaunchPlanError, Plan);
convert!(uuldana_transport::Error, Transport);
