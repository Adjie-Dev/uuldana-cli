# 0009. Native session identity may rotate

- Status: Accepted
- Date: 2026-10-12
- Amends: ADR 0006 (the rule that a recorded native session id cannot be replaced)

## Context

ADR 0006 made a run's native session id immutable once recorded, and `Session::record_native_session` returned an error for a different id. Real providers do not behave that way: an interactive agent can reset its conversation, resume an older session or fork a session without restarting, and resuming with a fork produces a new id. In the first runtime the conflict error stopped the whole run.

Adapter callbacks about past runs (`capture_session`, `prepare_handoff`) also received only a `RunRecord`, which does not identify the Uuldana session or workspace. Two runs from different workspaces could produce identical inputs, while many providers file their sessions per project directory.

## Decision

- Native session ids are provider-owned, opaque and **may change during a run**. No change is fatal.
- Each run keeps an ordered history of `NativeSessionEntry { id, link }`. `NativeSessionLink` is `New` (fresh or reset), `Resumed` (existing session continued), `Forked` (derived from the previous entry) or `Unspecified`. The current id is the last entry; the entry before a `Forked` entry is its parent.
- `Session::observe_native_session(run, native, link)` returns `First`, `Unchanged` or `Changed { previous }`. Its only error is an unknown run. Ended runs accept observations, so the result of a later capture can be recorded. `record_native_session` remains as the `Unspecified` shorthand. `SessionError::NativeSessionConflict` is removed.
- Decoders emit `AgentEvent::NativeSessionChanged { native, link }` when the provider reveals the relationship, and `NativeSessionIdentified { native }` otherwise. An invalid id is a `ProtocolViolation` event.
- Adapter callbacks about an existing run receive a `RunContext` (session id, absolute workspace, run), obtainable only from `Session::run_context`. `SessionCapture` gains an optional `parent`, and `HandoffPackage` gains an optional `source` plus `HandoffPackage::for_run`.

## Consequences

- Native session handling can never stop a run or trigger a cleanup loop.
- Lineage is recorded only as reliably as each provider reports it; Uuldana does not assume one session model for all providers.
- `capture_session` and `prepare_handoff` signatures changed (contract 0.2). Neither had overrides or callers in the workspace at the time of the change.
- History length is not bounded by the core; retention is part of the later persistence work.
