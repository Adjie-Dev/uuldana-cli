//! Transport descriptions and frames.
//!
//! A transport moves bytes or messages between Uuldana and a provider
//! process. This module only *describes* transports. Spawning processes,
//! allocating pseudo terminals and speaking JSON-RPC are runtime concerns
//! implemented outside `uuldana-core`.

use std::fmt;

/// The family of transport used to talk to a provider.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TransportKind {
    /// The provider's own interactive terminal UI, hosted in a pseudo terminal
    /// (ConPTY on Windows, a Unix PTY elsewhere). Bytes are passed through.
    NativePty,
    /// Agent Client Protocol: JSON-RPC 2.0 with the provider as the Agent and
    /// Uuldana as the Client. Uuldana renders the UI, not the provider.
    Acp,
    /// A provider-specific non-interactive or structured CLI mode, for
    /// example a JSON lines output mode. Framing is described by the adapter.
    CustomCli,
}

impl TransportKind {
    /// Stable snake_case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativePty => "native_pty",
            Self::Acp => "acp",
            Self::CustomCli => "custom_cli",
        }
    }

    /// Whether this transport can carry the provider's own terminal UI.
    ///
    /// Only [`TransportKind::NativePty`] does. ACP and custom CLI modes carry
    /// structured data that Uuldana renders itself, so native UI fidelity is
    /// never assumed for them.
    pub const fn carries_native_terminal_ui(self) -> bool {
        matches!(self, Self::NativePty)
    }
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalSize {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
}

impl TerminalSize {
    /// Conventional 24x80 fallback when the host terminal size is unknown.
    pub const DEFAULT: Self = Self::new(24, 80);

    /// Creates a size. Use [`TerminalSize::is_valid`] before handing it to a PTY.
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }

    /// A pseudo terminal cannot have zero rows or columns.
    pub const fn is_valid(self) -> bool {
        self.rows > 0 && self.cols > 0
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Inclusive range of ACP major protocol versions a client accepts.
///
/// ACP versions are single integers (uint16) that only change on breaking
/// changes. See `docs/PROTOCOL_COMPATIBILITY.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AcpVersionRange {
    min: u16,
    max: u16,
}

impl AcpVersionRange {
    /// ACP protocol version 1 only.
    pub const V1: Self = Self { min: 1, max: 1 };

    /// Creates a range. Fails when `min > max`.
    pub const fn new(min: u16, max: u16) -> Result<Self, TransportError> {
        if min > max {
            return Err(TransportError::InvalidVersionRange { min, max });
        }
        Ok(Self { min, max })
    }

    /// Lowest accepted version.
    pub const fn min(self) -> u16 {
        self.min
    }

    /// Highest accepted version. This is the version a client sends in
    /// `initialize`, because ACP requires the latest supported version.
    pub const fn max(self) -> u16 {
        self.max
    }

    /// Whether `version` falls inside the range.
    pub const fn contains(self, version: u16) -> bool {
        version >= self.min && version <= self.max
    }
}

/// Applies ACP version negotiation from the client side.
///
/// The client offers `supported.max()`. An agent that supports it echoes it
/// back; otherwise it answers with the latest version it supports. The client
/// accepts the answer only when it is inside `supported`, and must otherwise
/// close the connection and tell the user.
pub fn negotiate_acp_version(
    supported: AcpVersionRange,
    agent_response: u16,
) -> Result<u16, TransportError> {
    if supported.contains(agent_response) {
        Ok(agent_response)
    } else {
        Err(TransportError::VersionMismatch {
            offered: supported.max(),
            received: agent_response,
            supported,
        })
    }
}

/// How a custom CLI frames its standard output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamFraming {
    /// Unframed bytes, delivered as [`TransportFrame::Output`].
    Raw,
    /// Newline-delimited records, each delivered as [`TransportFrame::Message`].
    Lines,
}

/// What the runtime does with the provider's standard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StdinMode {
    /// Standard input is closed immediately after spawn.
    Closed,
    /// Standard input stays open as a pipe owned by the runtime.
    Piped,
}

/// Concrete transport parameters chosen by an adapter for one launch.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportSpec {
    /// Host the provider's terminal UI in a pseudo terminal.
    NativePty {
        /// Initial terminal size.
        initial_size: TerminalSize,
    },
    /// Speak ACP over the child's standard streams.
    Acp {
        /// ACP major versions this launch accepts.
        versions: AcpVersionRange,
    },
    /// Provider-specific structured CLI mode.
    CustomCli {
        /// Framing of standard output.
        stdout: StreamFraming,
        /// Handling of standard input.
        stdin: StdinMode,
    },
}

impl TransportSpec {
    /// The transport family of this spec.
    pub const fn kind(&self) -> TransportKind {
        match self {
            Self::NativePty { .. } => TransportKind::NativePty,
            Self::Acp { .. } => TransportKind::Acp,
            Self::CustomCli { .. } => TransportKind::CustomCli,
        }
    }
}

/// How a provider process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExitInfo {
    /// Exit code, when the platform reports one.
    pub code: Option<i32>,
    /// Whether the platform considers the exit successful.
    pub success: bool,
}

impl ExitInfo {
    /// Builds exit information from a numeric code. `0` is success.
    pub const fn from_code(code: i32) -> Self {
        Self {
            code: Some(code),
            success: code == 0,
        }
    }
}

/// One unit of data delivered by a transport to an event decoder.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFrame<'a> {
    /// Bytes from the PTY or from unframed standard output.
    Output(&'a [u8]),
    /// Bytes from standard error, when the transport separates it.
    Stderr(&'a [u8]),
    /// One complete framed message, for example a JSON-RPC message or a
    /// JSON lines record, already split by the transport.
    Message(&'a str),
    /// The provider process exited. Sent exactly once, last.
    Exited(ExitInfo),
}

/// Errors produced by transport descriptions and negotiation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The agent answered with an ACP version outside the client's range.
    VersionMismatch {
        /// Version the client offered.
        offered: u16,
        /// Version the agent answered with.
        received: u16,
        /// Range the client accepts.
        supported: AcpVersionRange,
    },
    /// A version range with `min > max`.
    InvalidVersionRange {
        /// Requested minimum.
        min: u16,
        /// Requested maximum.
        max: u16,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VersionMismatch {
                offered,
                received,
                supported,
            } => write!(
                f,
                "ACP version mismatch: offered {offered}, agent answered {received}, \
                 supported range is {}..={}",
                supported.min(),
                supported.max()
            ),
            Self::InvalidVersionRange { min, max } => {
                write!(f, "invalid ACP version range {min}..={max}")
            }
        }
    }
}

impl std::error::Error for TransportError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_native_pty_carries_terminal_ui() {
        assert!(TransportKind::NativePty.carries_native_terminal_ui());
        assert!(!TransportKind::Acp.carries_native_terminal_ui());
        assert!(!TransportKind::CustomCli.carries_native_terminal_ui());
    }

    #[test]
    fn spec_reports_its_kind() {
        let pty = TransportSpec::NativePty {
            initial_size: TerminalSize::DEFAULT,
        };
        let acp = TransportSpec::Acp {
            versions: AcpVersionRange::V1,
        };
        let cli = TransportSpec::CustomCli {
            stdout: StreamFraming::Lines,
            stdin: StdinMode::Closed,
        };
        assert_eq!(pty.kind(), TransportKind::NativePty);
        assert_eq!(acp.kind(), TransportKind::Acp);
        assert_eq!(cli.kind(), TransportKind::CustomCli);
    }

    #[test]
    fn acp_negotiation_accepts_echoed_version() {
        assert_eq!(negotiate_acp_version(AcpVersionRange::V1, 1), Ok(1));
    }

    #[test]
    fn acp_negotiation_accepts_older_version_inside_range() {
        let range = AcpVersionRange::new(1, 2).unwrap();
        assert_eq!(range.max(), 2, "client must offer its latest version");
        assert_eq!(negotiate_acp_version(range, 1), Ok(1));
    }

    #[test]
    fn acp_negotiation_rejects_versions_outside_range() {
        let err = negotiate_acp_version(AcpVersionRange::V1, 2).unwrap_err();
        assert_eq!(
            err,
            TransportError::VersionMismatch {
                offered: 1,
                received: 2,
                supported: AcpVersionRange::V1,
            }
        );
        assert!(negotiate_acp_version(AcpVersionRange::V1, 0).is_err());
    }

    #[test]
    fn acp_range_rejects_inverted_bounds() {
        assert_eq!(
            AcpVersionRange::new(3, 2),
            Err(TransportError::InvalidVersionRange { min: 3, max: 2 })
        );
    }

    #[test]
    fn terminal_size_validity() {
        assert!(TerminalSize::DEFAULT.is_valid());
        assert!(!TerminalSize::new(0, 80).is_valid());
        assert!(!TerminalSize::new(24, 0).is_valid());
    }

    #[test]
    fn exit_info_from_code() {
        assert!(ExitInfo::from_code(0).success);
        assert!(!ExitInfo::from_code(1).success);
    }
}
