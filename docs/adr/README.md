# Architecture Decision Records

Each ADR records one decision, its context and its consequences. The decision text of an accepted ADR is not rewritten: a later ADR supersedes it, and factual corrections are appended as a dated "Amendment" section.

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-synchronous-io-free-core.md) | Synchronous, I/O-free core contracts | Accepted, amended 2026-10-12 |
| [0002](0002-adapter-transport-supervisor-separation.md) | Adapter describes, transport moves bytes, supervisor decides | Accepted, amended 2026-10-12 |
| [0003](0003-tri-state-capabilities.md) | Tri-state capabilities with evidence precedence | Accepted |
| [0004](0004-separate-approval-and-sandbox.md) | Separate approval and sandbox, no silent escalation | Accepted |
| [0005](0005-zero-dependency-core.md) | No third-party dependencies in the core | Accepted |
| [0006](0006-session-ownership-and-fidelity.md) | Uuldana-owned sessions, fidelity labels, explicit non-transferable state | Accepted, amended by 0009 |
| [0007](0007-crate-layout-and-contract-versioning.md) | Crate layout, naming and contract versioning | Accepted |
| [0008](0008-run-finalization-and-shutdown.md) | Run finalization, shutdown fallback and terminal precedence | Accepted |
| [0009](0009-native-session-identity.md) | Native session identity may rotate | Accepted |

## Template

```markdown
# NNNN. Title

- Status: Proposed | Accepted | Superseded by NNNN
- Date: YYYY-MM-DD

## Context
What forces are at play.

## Decision
What we decided.

## Consequences
What becomes easier, what becomes harder, what we must watch.
```
