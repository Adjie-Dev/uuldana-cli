//! Lifecycle of a single provider run.
//!
//! A run is one provider process (or one ACP connection) inside a Uuldana
//! session. Its state machine is explicit so the runtime cannot skip states
//! or revive a run that has ended.
//!
//! [`RunOutcome`] defines how the final [`RunEnd`] is chosen from the events
//! observed during the run and the way the process finished. Every runtime
//! uses it so the precedence rules are implemented once (ADR 0008).

use std::fmt;

use crate::error::ErrorClass;
use crate::event::{AgentEvent, LimitKind, StopReason};
use crate::transport::ExitInfo;

/// Why a run ended.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunEnd {
    /// The run finished its work normally.
    Completed,
    /// The provider process exited on its own.
    Exited {
        /// Exit code, when known.
        code: Option<i32>,
    },
    /// The provider hit a limit and stopped.
    LimitReached(LimitKind),
    /// The run failed.
    Failed(ErrorClass),
    /// The run was cancelled and the provider exited on its own within the
    /// shutdown plan, or the provider reported the cancellation itself.
    Cancelled,
    /// A stop was requested and Uuldana had to force-terminate the provider
    /// because it did not exit within the shutdown grace period.
    Terminated,
}

/// State of a provider run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    /// Planned but not started.
    Pending,
    /// The process is being spawned or the connection is being initialized.
    Launching,
    /// The provider is running.
    Running,
    /// Shutdown was requested and is in progress.
    Stopping,
    /// Terminal state.
    Ended(RunEnd),
}

/// An input to the run state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunTransition {
    /// Start launching (`Pending -> Launching`).
    Launch,
    /// The provider is up (`Launching -> Running`).
    Started,
    /// Begin shutdown (`Launching | Running -> Stopping`).
    StopRequested,
    /// The run ended (any non-terminal state `-> Ended`).
    End(RunEnd),
}

impl RunState {
    /// Whether the run has ended.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Ended(_))
    }

    /// Applies `transition`, returning the next state or an error for
    /// transitions the state machine does not allow.
    pub fn apply(self, transition: RunTransition) -> Result<RunState, LifecycleError> {
        use RunState::*;
        use RunTransition::*;
        match (self, transition) {
            (Ended(end), _) => Err(LifecycleError::AlreadyEnded { end }),
            (Pending, Launch) => Ok(Launching),
            (Launching, Started) => Ok(Running),
            (Launching | Running, StopRequested) => Ok(Stopping),
            (_, End(end)) => Ok(Ended(end)),
            (from, transition) => Err(LifecycleError::InvalidTransition { from, transition }),
        }
    }
}

/// Invalid lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    /// The transition is not allowed from `from`.
    InvalidTransition {
        /// Current state.
        from: RunState,
        /// Rejected transition.
        transition: RunTransition,
    },
    /// The run has already ended.
    AlreadyEnded {
        /// How it ended.
        end: RunEnd,
    },
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, transition } => {
                write!(f, "transition {transition:?} is not allowed from {from:?}")
            }
            Self::AlreadyEnded { end } => write!(f, "run already ended ({end:?})"),
        }
    }
}

impl std::error::Error for LifecycleError {}

/// How the supervisor's involvement with a provider process ended.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunFinish {
    /// No stop was requested and the direct child exited on its own.
    Exited(ExitInfo),
    /// A stop was requested and the provider exited within the shutdown plan.
    Stopped,
    /// A stop was requested and Uuldana force-terminated the provider after
    /// the grace period.
    Killed,
    /// Uuldana aborted the run because of a supervisor-side failure, for
    /// example a decoder panic or an unrecoverable transport error.
    Aborted(ErrorClass),
}

/// Accumulates the events of one run and computes its final [`RunEnd`].
///
/// Terminal signals are scoped to the current turn: `TurnStarted` and a
/// successful `TurnEnded(EndTurn)` clear earlier limit and error signals, so
/// a provider that recovers is not reported as failed. Within a turn the most
/// recent signal wins. A `ProviderError` of class `ProtocolViolation`
/// (one malformed record) is never terminal by itself.
///
/// Precedence in [`RunOutcome::finalize`], highest first:
///
/// 1. `Aborted(class)` gives `Failed(class)`.
/// 2. `Killed` gives `Terminated`.
/// 3. `Stopped` gives `Cancelled` (an explicit stop outranks provider signals).
/// 4. The pending turn signal (`LimitReached`, `Failed`, `Cancelled`).
/// 5. `Completed` when the last turn ended with `EndTurn` and the exit succeeded.
/// 6. `Exited { code }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOutcome {
    turn_completed: bool,
    signal: Option<RunEnd>,
}

impl RunOutcome {
    /// Creates an empty outcome.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one normalized event. Events that do not affect the outcome
    /// are ignored.
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStarted => {
                self.turn_completed = false;
                self.signal = None;
            }
            AgentEvent::TurnEnded { reason } => match reason {
                StopReason::EndTurn => {
                    self.turn_completed = true;
                    self.signal = None;
                }
                StopReason::Cancelled => {
                    self.turn_completed = false;
                    self.signal = Some(RunEnd::Cancelled);
                }
                StopReason::LimitReached => {
                    self.turn_completed = false;
                    if !matches!(self.signal, Some(RunEnd::LimitReached(_))) {
                        self.signal = Some(RunEnd::LimitReached(LimitKind::Unknown));
                    }
                }
                _ => self.turn_completed = false,
            },
            AgentEvent::LimitReached(signal) => {
                self.signal = Some(RunEnd::LimitReached(signal.kind));
            }
            AgentEvent::ProviderError { class, .. } if *class != ErrorClass::ProtocolViolation => {
                self.signal = Some(RunEnd::Failed(*class));
            }
            _ => {}
        }
    }

    /// The signal that would decide a natural exit, if any.
    pub fn pending_signal(&self) -> Option<RunEnd> {
        self.signal
    }

    /// Computes the final [`RunEnd`] for `finish`.
    pub fn finalize(&self, finish: RunFinish) -> RunEnd {
        match finish {
            RunFinish::Aborted(class) => RunEnd::Failed(class),
            RunFinish::Killed => RunEnd::Terminated,
            RunFinish::Stopped => RunEnd::Cancelled,
            RunFinish::Exited(exit) => match self.signal {
                Some(signal) => signal,
                None if self.turn_completed && exit.success => RunEnd::Completed,
                None => RunEnd::Exited { code: exit.code },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::LimitSignal;

    #[test]
    fn happy_path() {
        let state = RunState::Pending
            .apply(RunTransition::Launch)
            .and_then(|s| s.apply(RunTransition::Started))
            .and_then(|s| s.apply(RunTransition::StopRequested))
            .and_then(|s| s.apply(RunTransition::End(RunEnd::Completed)))
            .unwrap();
        assert_eq!(state, RunState::Ended(RunEnd::Completed));
        assert!(state.is_terminal());
    }

    #[test]
    fn launch_failure_ends_directly() {
        let failed = RunState::Launching
            .apply(RunTransition::End(RunEnd::Failed(ErrorClass::NotInstalled)))
            .unwrap();
        assert_eq!(
            failed,
            RunState::Ended(RunEnd::Failed(ErrorClass::NotInstalled))
        );
    }

    #[test]
    fn running_can_end_on_limit() {
        let state = RunState::Running
            .apply(RunTransition::End(RunEnd::LimitReached(
                LimitKind::UsageQuota,
            )))
            .unwrap();
        assert!(state.is_terminal());
    }

    #[test]
    fn pending_can_be_cancelled() {
        assert_eq!(
            RunState::Pending.apply(RunTransition::End(RunEnd::Cancelled)),
            Ok(RunState::Ended(RunEnd::Cancelled))
        );
    }

    #[test]
    fn states_cannot_be_skipped() {
        let invalid = [
            (RunState::Pending, RunTransition::Started),
            (RunState::Pending, RunTransition::StopRequested),
            (RunState::Launching, RunTransition::Launch),
            (RunState::Running, RunTransition::Launch),
            (RunState::Running, RunTransition::Started),
            (RunState::Stopping, RunTransition::Started),
            (RunState::Stopping, RunTransition::StopRequested),
        ];
        for (from, transition) in invalid {
            assert_eq!(
                from.apply(transition),
                Err(LifecycleError::InvalidTransition { from, transition }),
                "{from:?} + {transition:?}"
            );
        }
    }

    #[test]
    fn ended_is_terminal() {
        let ended = RunState::Ended(RunEnd::Exited { code: Some(0) });
        for transition in [
            RunTransition::Launch,
            RunTransition::Started,
            RunTransition::StopRequested,
            RunTransition::End(RunEnd::Completed),
        ] {
            assert!(matches!(
                ended.apply(transition),
                Err(LifecycleError::AlreadyEnded { .. })
            ));
        }
    }

    #[test]
    fn stopping_can_end_terminated() {
        assert_eq!(
            RunState::Stopping.apply(RunTransition::End(RunEnd::Terminated)),
            Ok(RunState::Ended(RunEnd::Terminated))
        );
    }

    fn turn_start() -> AgentEvent {
        AgentEvent::TurnStarted
    }

    fn turn_end(reason: StopReason) -> AgentEvent {
        AgentEvent::TurnEnded { reason }
    }

    fn limit(kind: LimitKind) -> AgentEvent {
        AgentEvent::LimitReached(LimitSignal {
            kind,
            resets_at: None,
            message: None,
        })
    }

    fn error(class: ErrorClass) -> AgentEvent {
        AgentEvent::ProviderError {
            class,
            message: String::new(),
        }
    }

    fn text() -> AgentEvent {
        AgentEvent::MessageChunk {
            role: crate::event::Role::Agent,
            text: "progress".into(),
        }
    }

    fn outcome(events: &[AgentEvent]) -> RunOutcome {
        let mut outcome = RunOutcome::new();
        for event in events {
            outcome.observe(event);
        }
        outcome
    }

    fn exited(code: i32) -> RunFinish {
        RunFinish::Exited(ExitInfo::from_code(code))
    }

    #[test]
    fn mock_protocol_scenarios_keep_their_expected_outcomes() {
        // Mirrors the deterministic mock provider scenarios used by the runtime.
        let cases = [
            (
                vec![turn_start(), text(), turn_end(StopReason::EndTurn)],
                exited(0),
                RunEnd::Completed,
            ),
            (
                vec![turn_start(), turn_end(StopReason::EndTurn), turn_start()],
                exited(0),
                RunEnd::Exited { code: Some(0) },
            ),
            (
                vec![turn_start(), text(), limit(LimitKind::UsageQuota)],
                exited(1),
                RunEnd::LimitReached(LimitKind::UsageQuota),
            ),
            (
                vec![turn_start(), text(), limit(LimitKind::RateLimit)],
                exited(1),
                RunEnd::LimitReached(LimitKind::RateLimit),
            ),
            (
                vec![turn_start(), text(), limit(LimitKind::ContextWindow)],
                exited(1),
                RunEnd::LimitReached(LimitKind::ContextWindow),
            ),
            (
                vec![turn_start(), error(ErrorClass::AuthenticationRequired)],
                exited(1),
                RunEnd::Failed(ErrorClass::AuthenticationRequired),
            ),
            (
                vec![turn_start(), error(ErrorClass::Transient)],
                exited(1),
                RunEnd::Failed(ErrorClass::Transient),
            ),
            (
                vec![turn_start(), error(ErrorClass::Internal)],
                exited(1),
                RunEnd::Failed(ErrorClass::Internal),
            ),
            (
                vec![turn_start(), text()],
                exited(23),
                RunEnd::Exited { code: Some(23) },
            ),
            (
                vec![turn_start(), text(), turn_end(StopReason::Cancelled)],
                RunFinish::Stopped,
                RunEnd::Cancelled,
            ),
        ];
        for (events, finish, expected) in cases {
            assert_eq!(
                outcome(&events).finalize(finish),
                expected,
                "{events:?} {finish:?}"
            );
        }
    }

    #[test]
    fn malformed_record_is_not_terminal() {
        let events = [
            turn_start(),
            error(ErrorClass::ProtocolViolation),
            turn_end(StopReason::EndTurn),
        ];
        assert_eq!(outcome(&events).finalize(exited(0)), RunEnd::Completed);
        let only_malformed = [turn_start(), error(ErrorClass::ProtocolViolation)];
        assert_eq!(
            outcome(&only_malformed).finalize(exited(2)),
            RunEnd::Exited { code: Some(2) }
        );
    }

    #[test]
    fn recovered_signals_are_cleared_by_end_turn_or_next_turn() {
        let recovered = [
            turn_start(),
            limit(LimitKind::RateLimit),
            error(ErrorClass::Transient),
            turn_end(StopReason::EndTurn),
        ];
        assert_eq!(outcome(&recovered).finalize(exited(0)), RunEnd::Completed);
        let next_turn = [turn_start(), limit(LimitKind::RateLimit), turn_start()];
        let o = outcome(&next_turn);
        assert_eq!(o.pending_signal(), None);
        assert_eq!(o.finalize(exited(0)), RunEnd::Exited { code: Some(0) });
    }

    #[test]
    fn latest_signal_in_turn_wins() {
        let events = [
            turn_start(),
            error(ErrorClass::Transient),
            limit(LimitKind::UsageQuota),
        ];
        assert_eq!(
            outcome(&events).finalize(exited(1)),
            RunEnd::LimitReached(LimitKind::UsageQuota)
        );
    }

    #[test]
    fn stop_reason_limit_keeps_specific_kind() {
        let specific = [
            turn_start(),
            limit(LimitKind::UsageQuota),
            turn_end(StopReason::LimitReached),
        ];
        assert_eq!(
            outcome(&specific).finalize(exited(1)),
            RunEnd::LimitReached(LimitKind::UsageQuota)
        );
        let generic = [turn_start(), turn_end(StopReason::LimitReached)];
        assert_eq!(
            outcome(&generic).finalize(exited(1)),
            RunEnd::LimitReached(LimitKind::Unknown)
        );
    }

    #[test]
    fn supervisor_decisions_outrank_provider_signals() {
        let quota = outcome(&[turn_start(), limit(LimitKind::UsageQuota)]);
        assert_eq!(quota.finalize(RunFinish::Stopped), RunEnd::Cancelled);
        assert_eq!(quota.finalize(RunFinish::Killed), RunEnd::Terminated);
        assert_eq!(
            quota.finalize(RunFinish::Aborted(ErrorClass::Internal)),
            RunEnd::Failed(ErrorClass::Internal)
        );
        let completed = outcome(&[turn_start(), turn_end(StopReason::EndTurn)]);
        assert_eq!(completed.finalize(RunFinish::Killed), RunEnd::Terminated);
    }

    #[test]
    fn completion_requires_successful_exit() {
        let completed = outcome(&[turn_start(), turn_end(StopReason::EndTurn)]);
        assert_eq!(
            completed.finalize(exited(3)),
            RunEnd::Exited { code: Some(3) }
        );
        let no_turns = RunOutcome::new();
        assert_eq!(
            no_turns.finalize(exited(0)),
            RunEnd::Exited { code: Some(0) }
        );
    }
}
