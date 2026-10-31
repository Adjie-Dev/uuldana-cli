# 0002. Adapter describes, transport moves bytes, supervisor decides

- Status: Accepted
- Date: 2026-10-12

## Context

The project must support native PTY, ACP and custom CLI integrations, keep provider-specific logic out of the core, and never replace a provider's agent loop. Responsibilities for providers, transports, supervision, sessions, events and permissions must be separable and independently testable.

## Decision

Three roles with one-way data flow:

1. **Adapter** (`ProviderAdapter`): knows one provider. Produces descriptions only: an `Installation`, a `CapabilitySet`, a `PermissionMapping`, a `LaunchPlan` with a `TransportSpec`, and an `EventDecoder`.
2. **Transport** (runtime crates): knows one wire mechanism. Spawns the process described by a `LaunchPlan`, delivers `TransportFrame`s to the decoder, sends input, resizes and shuts down.
3. **Supervisor** (runtime): owns sessions, permissions, lifecycle and the event log. Validates every mapping and plan through core functions before acting.

Transports are modeled independently of adapters: `TransportKind`/`TransportSpec` say *which* mechanism and its parameters; nothing in a transport depends on a provider.

## Consequences

- Native PTY and ACP can be implemented, tested and replaced independently.
- Generic protocol logic (ACP) is written once in the transport layer; adapters only override it for vendor extensions.
- Adapters cannot perform side effects, which makes the "do not modify native configuration" rule structural.
- An adapter's launch arguments are opaque strings to the core. Ensuring they match the validated permission policy relies on adapter review and conformance tests.

## Amendment 2026-10-12 (contract 0.2)

The consequence "Adapters cannot perform side effects, which makes the 'do not modify native configuration' rule structural" overstated the guarantee. A `LaunchPlan` names an arbitrary executable that can write wherever the operating system allows, and in-process adapters are ordinary Rust code. The separation still keeps file-writing APIs out of the core, but leaving native configuration, authentication and unrelated files untouched is a responsibility of trusted adapters, enforced by review and conformance tests rather than by types.
