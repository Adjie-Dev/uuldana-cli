# 0005. No third-party dependencies in the core

- Status: Accepted
- Date: 2026-10-12

## Context

`uuldana-core` is the contract every adapter and runtime crate depends on. Each dependency it pulls in becomes a transitive dependency of every adapter, affects build times and supply-chain exposure, and can force version conflicts. The core needs no serialization, async runtime or date handling: contracts are in-memory types.

## Decision

- `uuldana-core` depends only on `std`.
- Errors implement `Display` and `std::error::Error` by hand instead of using a derive crate.
- Timestamps are milliseconds since the Unix epoch (`Timestamp`), not a date library type.
- Structured payloads that the core does not interpret (vendor extensions) are opaque strings.

## Consequences

- The core builds offline and quickly, and adapter authors inherit nothing they did not choose.
- Persisting sessions and events (unified history) will require serialization. `serde` support will be added as an **optional** feature with an explicit on-disk schema version. Adding derives behind a feature is additive and does not break the contract.
- Runtime and transport crates are free to use `tokio`, `serde_json`, PTY crates, and so on.
