# 0003. Tri-state capabilities with evidence precedence

- Status: Accepted
- Date: 2026-10-12

## Context

Providers differ widely and change between versions. Treating "not mentioned" as "supported" leads to failures at the worst moment (for example during a failover). Treating it as "unsupported" hides features. ACP negotiates some capabilities at connect time, which may contradict an adapter's static assumptions. ACP also does not carry a provider's native terminal UI.

## Decision

- Capability support is tri-state: `Supported`, `Unsupported`, `Unknown`. A capability nobody stated is `Unknown`, and only `Supported` passes `CapabilitySet::require`.
- Every statement records its evidence: `Declared` < `Probed` < `Negotiated`. During refinement, a statement replaces the current one only if its evidence rank is equal or higher.
- Transport rules are applied last. Any transport other than `NativePty` forces `NativeTerminalUi` to `Unsupported`.
- Third-party capabilities use namespaced names (`vendor.feature`) through `Capability::Custom`.

## Consequences

- Unsupported behavior is always explicit and typed, including default trait methods that return `AdapterError::unsupported(capability)`.
- The supervisor can choose transports and failover targets by requiring capabilities instead of hardcoding providers.
- An explicit `Unknown` from a higher-ranked source downgrades a lower-ranked `Supported`; this is intentionally conservative.
