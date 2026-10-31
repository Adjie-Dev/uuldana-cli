# Uuldana Provider SDK

This guide explains how to integrate a coding agent ("provider") with Uuldana by writing a **provider adapter**. Adapters depend on the `uuldana-provider-sdk` crate, which re-exports the stable adapter-facing part of `uuldana-core`.

New providers never require changes to the core contracts. If you find you need one, open an issue describing the capability; until then, use a namespaced custom capability or an extension event.

## 1. What an adapter is

An adapter is a small, synchronous, side-effect free description of one provider:

| Method | Answers | Required |
| --- | --- | --- |
| `descriptor` | Who are you, which contract version, which transports? | yes |
| `detect` | Is the provider installed, where, which version? | yes |
| `capabilities` | What can this installation do? | yes |
| `map_permissions` | What does a requested permission policy really become on this provider? | yes |
| `launch_plan` | How do I start a fresh session? | yes |
| `resume_plan` | How do I resume a native session? | default: `Unsupported` |
| `event_decoder` | How do I turn output into normalized events? | yes (may return `None`) |
| `capture_session(context, probe)` | Which native session did a run produce, in which workspace? | default: `Unsupported` |
| `prepare_handoff(context)` | What provider-specific visible context helps a handoff? | default: `Unsupported` |
| `shutdown_plan` | How do I stop gracefully? | default: interrupt, 5 s grace |

An adapter never spawns a process, owns a transport, writes a file, or keeps per-run mutable state outside its decoder. The runtime executes what the adapter describes.

Adapters are **trusted code**. The core cannot stop an adapter, or the executable it launches, from writing files: a `LaunchPlan` names an arbitrary program. The rules below are obligations that review and conformance tests check, not guarantees enforced by types.

## 2. Mandatory rules

These rules protect the user's native setup and are part of the contract. Adapters that break them will not be accepted.

1. **Preserve native behavior.** Launch the provider's own binary in its normal mode. Do not replace its agent loop, tools, MCP servers, skills, plugins, configuration or instruction-file discovery. Do not pass flags that disable discovery (for example "bare" or "safe" modes) unless the user explicitly asked for it.
2. **Never modify configuration or authentication.** Describe only the program, arguments, child environment and working directory. Do not point the provider at a different configuration directory through environment variables, do not launch helper programs that write settings, and do not write settings files from adapter code.
3. **Never read credentials.** Detection may locate the executable and run a version command. It must not open token, key or auth files.
4. **Report permissions honestly.** In `map_permissions`, if the provider cannot enforce a dimension, return the permissive value that will really apply. The core then refuses the launch instead of running with more permission than requested. Mapping to a stricter value is always fine.
5. **Launch with the effective policy only.** In `launch_plan`, translate `request.permissions.effective()` into provider flags. Never add flags that are more permissive than that policy.
6. **Decoders must not panic.** Malformed input becomes `AgentEvent::ProviderError { class: ErrorClass::ProtocolViolation, .. }` or `AgentEvent::Diagnostic`, and decoding continues with the next record. A protocol violation is never terminal by itself. Use other error classes only for errors the provider actually reported.
7. **Label fidelity truthfully.** Use `DecodedEvent::native` only for structured, documented messages. Anything derived from terminal text or guesswork is `DecodedEvent::inferred`.
8. **No hidden state claims.** Only emit `ThoughtChunk` for reasoning the provider displays. Never present undocumented internal state as transferable.
9. **Unknown is not supported.** If you do not know whether a capability works on an installed version, leave it out or declare it `Unknown`.
10. **Keep detection cheap and inert.** Use `HostProbe::run` only for short, read-only commands with a timeout.
11. **Do not assume session semantics.** Native session ids may change during a run. Report what the provider says and leave lineage `Unspecified` when the provider does not say (see 3.7).

## 3. Step by step

### 3.1 Dependency

```toml
[dependencies]
uuldana-provider-sdk = "0.1"
```

Inside this workspace use `uuldana-provider-sdk.workspace = true`.

### 3.2 Descriptor

```rust
use uuldana_provider_sdk::prelude::*;

pub struct ExampleAgent {
    descriptor: ProviderDescriptor,
}

impl ExampleAgent {
    pub fn new() -> Self {
        Self {
            descriptor: ProviderDescriptor::new(
                ProviderId::new("example-agent").expect("valid id"),
                "Example Agent",
                vec![TransportKind::NativePty, TransportKind::CustomCli],
            ),
        }
    }
}
```

Provider ids are lowercase ASCII (`a-z`, `0-9`, `-`, `_`, `.`), at most 64 bytes. `ProviderDescriptor::new` stamps the contract version your adapter was compiled against.

### 3.3 Detection and capabilities

```rust
fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
    let exe = probe
        .find_executable("example-agent")
        .ok_or_else(|| AdapterError::new(ErrorClass::NotInstalled, "example-agent not found"))?;
    let out = probe.run(&exe, &["--version"], std::time::Duration::from_secs(5))?;
    Ok(Installation::new(exe).with_version(String::from_utf8_lossy(&out.stdout).trim()))
}

fn capabilities(&self, _installation: &Installation) -> CapabilitySet {
    CapabilitySet::new()
        .with(Capability::NativeTerminalUi, Support::Supported, Evidence::Declared)
        .with(Capability::NetworkSandboxControl, Support::Unsupported, Evidence::Declared)
}
```

Use `Evidence::Probed` for statements that depend on the detected version. The supervisor refines your statements with `Evidence::Negotiated` data from the live connection and applies transport rules last (no transport except `NativePty` can claim `NativeTerminalUi`).

### 3.4 Permissions

```rust
fn map_permissions(&self, _inst: &Installation, requested: &PermissionPolicy) -> PermissionMapping {
    let mut effective = *requested;
    // This provider has no network sandbox: say so.
    effective.sandbox.network = uuldana_provider_sdk::NetworkAccess::Allowed;
    PermissionMapping::new(effective).with_note("no network sandbox")
}
```

The supervisor calls `validate_mapping(requested, &mapping)`. If `effective` is more permissive than `requested` in any dimension, validation fails with `PermissionError::WouldEscalate` and nothing is launched.

### 3.5 Launch plan

```rust
fn launch_plan(&self, inst: &Installation, req: &LaunchRequest) -> Result<LaunchPlan, AdapterError> {
    let transport = match req.transport {
        TransportKind::NativePty => TransportSpec::NativePty {
            initial_size: req.terminal_size.unwrap_or_default(),
        },
        other => {
            return Err(AdapterError::new(
                ErrorClass::Unsupported,
                format!("transport {other} is not supported"),
            ))
        }
    };
    Ok(LaunchPlan::new(inst.executable.clone(), req.workspace.clone(), transport)
        .args(["--approval", "ask"]))
}
```

The supervisor checks every plan with `LaunchPlan::validate`: absolute program and working directory, working directory equal to the workspace, transport matching the request, valid PTY size, no NUL in arguments, well-formed environment changes.

Use `LaunchPlan::native_session` to say how the native session id becomes known: `Preassigned(id)` if the provider accepts an id at launch, `DiscoverFromEvents` if your decoder emits `NativeSessionIdentified`, otherwise `Unavailable`.

Terminal size: `TerminalSize::is_valid` only checks that both dimensions are non-zero. Backend limits (ConPTY accepts at most 32767 rows or columns because its size is a signed 16-bit `COORD`) are enforced by the transport, not by adapters. Pass the requested size through; do not clamp it yourself.

### 3.6 Event decoder

```rust
struct MyDecoder;

impl EventDecoder for MyDecoder {
    fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>) {
        match frame {
            TransportFrame::Message(line) => { /* parse one structured record */ }
            TransportFrame::Output(bytes) => { /* PTY bytes: heuristics only, Inferred */ }
            TransportFrame::Exited(exit) => {
                out.push(DecodedEvent::native(AgentEvent::ProcessExited(exit)));
            }
            _ => {} // TransportFrame is non_exhaustive
        }
    }
}
```

`event_decoder(TransportKind::Acp)` may return `None`: the runtime then uses the shared ACP mapping. Return a decoder for ACP only to interpret vendor `_meta` or extension methods.

Limit detection belongs in the decoder: emit `AgentEvent::LimitReached(LimitSignal { kind, resets_at, message })`. Choose `LimitKind` carefully, because it drives the default disposition: only `UsageQuota` is eligible for failover; `RateLimit` retries the same provider; `Budget` (a user-set spending cap) requires the user.

How your events decide the run outcome: the runtime feeds every event to `RunOutcome`. Limit and error signals only count for the current turn; `TurnStarted` and `TurnEnded { reason: EndTurn }` clear them. So emit `TurnStarted` and `TurnEnded` whenever the provider exposes turn boundaries, and emit `LimitReached` or `ProviderError` only for conditions the provider actually reported. A malformed record (`ProtocolViolation`) never decides the outcome.

### 3.7 Native sessions

Native session ids belong to the provider and may change during a run. Report them as follows:

| Situation | Event |
| --- | --- |
| The provider tells you its session id | `NativeSessionIdentified { native }` |
| The provider started a fresh session in the same process (reset, clear, new conversation) | `NativeSessionChanged { native, link: NativeSessionLink::New }` |
| The provider switched to an existing session (resume or load inside the run) | `NativeSessionChanged { native, link: NativeSessionLink::Resumed }` |
| The provider forked or branched the current session | `NativeSessionChanged { native, link: NativeSessionLink::Forked }` |
| The id changed and the provider does not say why | `NativeSessionIdentified { native }` (recorded as `Unspecified`) |

The supervisor records every id in order (`RunRecord::native_sessions`); the entry before a `Forked` entry is its parent. Repeating the current id is harmless. No id change is ever fatal, and a value that is not a valid `NativeSessionId` is a `ProtocolViolation` event, not an error. Do not assume another provider has the same session model.

### 3.8 Shutdown

`shutdown_plan` returns ordered steps and a grace period. The runtime runs each step; if the transport cannot perform one (for example `Interrupt` on a plain pipe), it records a diagnostic and tries `ShutdownStep::fallback()` once (`Interrupt` falls back to `CloseInput`) without spending the grace period. If the provider exits within `grace`, the run ends `Cancelled`; otherwise Uuldana force-terminates it and the run ends `Terminated`. Prefer steps the provider handles natively, such as writing its documented exit command with `WriteInput`.

### 3.9 Session capture and handoff

Both callbacks receive a `RunContext` instead of a bare run record, because identical run records can come from different sessions and workspaces:

```rust
fn capture_session(
    &self,
    context: &RunContext<'_>,
    probe: &dyn HostProbe,
) -> Result<SessionCapture, AdapterError> {
    // Read-only lookup keyed by context.workspace() and the run's native ids.
    let current = context.run().native_session().cloned();
    Ok(SessionCapture::new(current, Support::Unknown))
}

fn prepare_handoff(&self, context: &RunContext<'_>) -> Result<HandoffPackage, AdapterError> {
    let package = HandoffPackage::for_run(context); // workspace + source session
    Ok(package)
}
```

Limits you must respect:

- Capture is best effort and read-only. Return `None` or `Support::Unknown` rather than guessing. Use `SessionCapture::with_parent` only when the provider records the lineage.
- Never read credential files while looking for sessions, and never modify the provider's session store.
- A handoff carries visible context only. `HandoffPackage::not_transferred()` always lists hidden reasoning, native memory, provider approvals, internal state and in-flight tool calls; do not try to work around it.
- A native session id is meaningful only to the provider that issued it.

### 3.10 Registration

```rust
let mut registry = uuldana_core::ProviderRegistry::new();
registry.register(std::sync::Arc::new(ExampleAgent::new()))?;
```

Registration fails for duplicate ids, adapters with no transport, or an incompatible contract version.

A complete, tested example lives in `crates/uuldana-provider-sdk/tests/example_adapter.rs`.

## 4. Choosing a transport

| Transport | Use when | Consequences |
| --- | --- | --- |
| `NativePty` | The provider's interactive terminal UI should be shown unchanged | Full native fidelity. Events are mostly `Inferred`. Approvals happen inside the provider UI. |
| `Acp` | The provider (or an official adapter for it) speaks the Agent Client Protocol | Structured `Native` events and permission requests Uuldana can answer. No native terminal UI. |
| `CustomCli` | The provider has a documented structured CLI mode (for example JSON lines) | Structured events, usually non-interactive. Framing declared in `TransportSpec::CustomCli`. |

An adapter may support several transports; list them in preference order in the descriptor.

## 5. Versioning and compatibility

- The adapter contract has its own version, `CONTRACT_VERSION` (currently `0.2`). While the major is `0`, any minor change is breaking and adapters must be rebuilt. The change log in `docs/CONTRACT_FREEZE.md` lists every change.
- Enums that will grow are `#[non_exhaustive]`: always include a wildcard arm when matching `TransportKind`, `TransportFrame`, `AgentEvent`, `ErrorClass`, `Capability`, `LimitKind`, `StopReason`, `ToolKind`, `RunEnd` or `NativeSessionLink`.
- Structs you receive (`LaunchRequest`, `ResumeRequest`) or build (`LaunchPlan`, `Installation`, `ProviderDescriptor`) are `#[non_exhaustive]`; use their constructors and builder methods.
- New trait methods will always arrive with a default implementation.

## 6. Review checklist

- [ ] No provider-specific code was added to `uuldana-core`.
- [ ] `detect` only finds the executable and runs a short version command.
- [ ] No file writes, no credential reads, no configuration directory overrides.
- [ ] `map_permissions` reports permissive values for anything the provider cannot enforce.
- [ ] Native session changes are reported with the right `NativeSessionLink`, or as `NativeSessionIdentified` when unknown.
- [ ] `capture_session` and `prepare_handoff`, if implemented, use the `RunContext` workspace and stay read-only.
- [ ] `launch_plan` derives flags from `request.permissions.effective()` only.
- [ ] Unknown capabilities are `Unknown`, not `Supported`.
- [ ] Decoder handles malformed input without panicking and labels fidelity truthfully.
- [ ] Tests cover detection, permission mapping, plan validation and decoding.

## Provider manifests

A provider that can be launched in its own terminal UI and described by flags does not need a compiled adapter. Put a JSON manifest in the `adapters` directory next to `config.json` (see [Configuration](CONFIGURATION.md)). Uuldana loads every `*.json` file there at start. A manifest that cannot be used is reported as `adapter not loaded: <file>: <reason>` and never hides the others. A manifest id that duplicates another adapter is refused.

```json
{
  "version": 1,
  "id": "my-agent",
  "name": "My Agent",
  "executables": ["my-agent"],
  "version_args": ["--version"],
  "version_contains": "my-agent",
  "modes": [
    { "approval": "never_ask", "filesystem": "unrestricted", "network": "allowed",
      "args": ["--yes"] },
    { "approval": "ask_every_action", "filesystem": "unrestricted", "network": "allowed",
      "args": [] }
  ],
  "prompt_args": ["--prompt", "{prompt}"],
  "resume_args": ["--resume", "{session}"],
  "session_id_args": ["--session-id", "{session}"],
  "interrupts": 2,
  "limit_phrases": ["quota exhausted"]
}
```

| Field | Meaning |
| --- | --- |
| `id` | Provider id used in `config.json`. Lowercase ASCII, digits, `-`, `_`, `.`. |
| `executables` | Names to look for on `PATH`. Script shims (`.cmd`, `.bat`, `.ps1`) are refused because the pseudo terminal cannot host them safely; point at the native executable. |
| `version_args` | Arguments that print the version. Default `["--version"]`. |
| `version_contains` | Text the version output must contain (case-insensitive). Without it, any output with a version number is accepted. |
| `modes` | Permission behaviors, ordered from the most to the least permissive. Each lists the policy the flags really enforce, the `args` that select it, and optionally a `note`. Uuldana picks the first mode that stays inside the requested policy. If none does, the entry is refused, which is the honest answer for a provider that cannot enforce a restriction. |
| `leading_args` | Arguments that come first on every start, for a program that needs a subcommand before anything else. |
| `prompt_args` | How to pass a first prompt, with `{prompt}`. Without it the provider is reported as not accepting an initial prompt, and a handoff to it is refused. |
| `resume_args` | How to resume a native session, with `{session}`. Without it, resume is unsupported. |
| `continue_args` | How to continue the latest conversation of the folder without a session id. Without it, that kind of resume is unsupported. |
| `session_id_args` | How to choose the session id at launch, with `{session}`. Uuldana then knows the id without looking for it. |
| `interrupts` | How many Ctrl+C presses stop the provider gracefully (1 to 3). Default 1. |
| `limit_phrases` | Texts the provider prints when its usage limit is reached. A match marks the run as out of quota, inferred from terminal text. |

A manifest never runs code. It names an executable that Uuldana launches, so only install manifests you trust. See `examples/adapters/my-agent.json` and `examples/config.json`.

## Compiled adapter example

`crates/uuldana-provider-sdk/examples/minimal_adapter.rs` is a complete adapter in about eighty lines, registered and discovered through the same path as the built-in ones:

```
cargo run -p uuldana-provider-sdk --example minimal_adapter
```
