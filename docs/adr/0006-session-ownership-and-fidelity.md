# 0006. Uuldana-owned sessions, fidelity labels, explicit non-transferable state

- Status: Accepted
- Date: 2026-10-12

## Context

Uuldana promises unified cross-provider session history and, later, continuation of unfinished work on another provider. Providers keep their own transcripts, memories, caches and hidden reasoning, in formats that are undocumented or change without notice. Some observations are precise (structured protocol messages) and some are guesses (matching text in a terminal).

## Decision

- Uuldana owns a `Session` record with ordered `RunRecord`s. Each run references the provider's native session only through an opaque `NativeSessionId`, which cannot be replaced once recorded.
- Uuldana never stores or rewrites provider transcripts as its own; its history is the normalized event log.
- Every event carries a `Fidelity`: `Native`, `Inferred` or `Synthetic`.
- `HandoffPackage` always lists what it does not carry (`NonTransferable::ALL`): hidden reasoning, native memory, provider approvals, provider internal state, in-flight tool calls. The list cannot be cleared.
- `ThoughtChunk` events only contain reasoning that the provider displayed.

## Consequences

- Consumers can weigh evidence: a failover policy can require `Native` limit signals or ask the user when the signal is `Inferred`.
- The project never claims that hidden or undocumented state moves between providers.
- Session invariants (one active run, ceiling, lifecycle) live in the core and are unit-tested, so every runtime enforces them identically.

## Amendment 2026-10-12 (contract 0.2)

The rule that a run's native session id "cannot be replaced once recorded" is superseded by ADR 0009: native ids may rotate within a run and every change is recorded in order.
