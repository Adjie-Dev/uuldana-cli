# 0008. Run finalization, shutdown fallback and terminal precedence

- Status: Accepted
- Date: 2026-10-12

## Context

A lifecycle review reproduced several lifecycle defects whose root cause was missing contract rules rather than a single runtime bug:

- A run stayed `Running` for as long as a descendant process held the provider's output stream, and a stop that had to force-kill the provider was reported as `Failed(ProcessFailed)`.
- One invalid UTF-8 line, or one malformed record, ended the whole run.
- The first error or limit signal of a run decided its outcome even after the provider recovered, so a completed run could be reported as failed or as a limit.
- The default `ShutdownPlan` (`Interrupt`) is unsupported on plain pipes, so every pipe-based stop waited the full grace period and was then force-killed.
- Cancellation and forced termination were both reported as `Cancelled`.

The future failover phase consumes `RunEnd`, so these rules must be identical in every runtime.

## Decision

1. **Finalization trigger.** A run is final when its direct child is reaped (or the connection is closed). The transport then drains remaining output for a bounded time and delivers exactly one `Exited` frame. Output streams held open by descendants never delay finalization. Errors during finalization are diagnostics and never start a second cleanup.
2. **Malformed data is non-terminal.** Decoder protocol violations and per-record framing errors are recorded as `ProviderError { class: ProtocolViolation }` events; the transport resynchronizes at the next record. Only an unusable transport or a decoder panic aborts a run.
3. **Shutdown fallback.** Steps run in order, each bounded by `grace`. An unsupported or failed step is recorded and its `ShutdownStep::fallback()` is tried once without consuming grace (`Interrupt` falls back to `CloseInput`). Then the runtime waits up to `grace`, then force-terminates. The fields and default of `ShutdownPlan` are unchanged.
4. **Distinct endings.** `RunEnd::Cancelled` means the provider exited within the shutdown plan (or reported the cancellation itself). The new `RunEnd::Terminated` means Uuldana force-terminated it.
5. **One precedence implementation.** `RunOutcome` in `uuldana-core` observes events and computes the final `RunEnd` from a `RunFinish` (`Exited(ExitInfo)`, `Stopped`, `Killed`, `Aborted(ErrorClass)`). Precedence: `Aborted` > `Killed` > `Stopped` > pending turn signal > `Completed` > `Exited { code }`. Signals are scoped to the current turn and cleared by `TurnStarted` and `TurnEnded(EndTurn)`; within a turn the latest wins; `ProtocolViolation` is ignored.

## Consequences

- Runtime behavior is defined by a pure, unit-tested core function instead of per-runtime logic.
- An explicit user stop always outranks provider signals, so a cancelled run is never failover eligible.
- `Terminated` is a new `RunEnd` variant; code that compared forced stops with `Cancelled` must be updated.
- Process-tree ownership (job objects, process groups) remains a runtime responsibility; this ADR only guarantees that descendants cannot block finalization.
