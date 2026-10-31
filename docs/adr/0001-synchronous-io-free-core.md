# 0001. Synchronous, I/O-free core contracts

- Status: Accepted
- Date: 2026-10-12

## Context

The supervisor will be asynchronous: it multiplexes a PTY or JSON-RPC stream, user input, timers and signals. Adapters are stored as `Arc<dyn ProviderAdapter>` in a registry, so the adapter trait must be dyn-compatible.

`async fn` in traits is stable (Rust 1.75) but not dyn-compatible; native dynamic dispatch for async methods is still a project goal for 2026 to 2027. The usual workaround, the `async-trait` crate, boxes every future and adds a dependency to every adapter.

Most adapter work does not need I/O at all: building a launch plan, mapping permissions and decoding frames are pure functions. Only detection needs to look at the host.

## Decision

- All traits in `uuldana-core` are synchronous.
- The core performs no I/O. Host access needed by adapters is injected through the read-only `HostProbe` trait, implemented by the runtime.
- Adapters return data (`LaunchPlan`, `PermissionMapping`, `CapabilitySet`, `DecodedEvent`s); the runtime performs the I/O.
- No async runtime (Tokio or other) is referenced by the core.

## Consequences

- Adapters are trivially unit-testable with fake probes and byte slices.
- The runtime may call `detect` on a blocking thread because `HostProbe::run` can wait for a child process.
- Long-running provider interaction is never inside an adapter method, so an adapter cannot block the supervisor loop for long.
- If a future capability genuinely needs async adapter code, it will be added as a separate optional trait rather than by changing `ProviderAdapter`.

## Amendment 2026-10-12 (contract 0.2)

`Timestamp::now()` read the system clock inside `uuldana-core`, which contradicted this ADR. It was removed. The core now offers only pure conversions (`Timestamp::from_unix_millis`, `Timestamp::from_system_time`); the runtime reads the clock at its boundary and passes timestamps in.
