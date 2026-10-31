# Uuldana Architecture

Uuldana is a universal, open-source native AI coding agent supervisor with automatic failover, seamless context handoff, and unified session history.

| | |
| --- | --- |
| Product | Uuldana |
| Repository | `uuldana-cli` |
| Executable | `uuldana` |
| Status | Contract 0.2. Runtime, transports, providers, sessions and failover are implemented on top of it; see section 16 |
| Contract version | `0.2` (`uuldana_core::CONTRACT_VERSION`) |

## 1. What Uuldana is and is not

Uuldana runs existing AI coding agents ("providers") through the interfaces those agents already ship: their interactive terminal UI, the Agent Client Protocol (ACP), or a provider-specific CLI mode. It supervises the process, observes what happens, keeps a provider-neutral history, and (in later phases) continues unfinished work on another provider when the active one hits a usage limit.

Uuldana is **not**:

- a new AI agent, LLM wrapper or generic agent harness;
- a replacement reasoning engine or agent loop;
- a re-implementation of provider tools, MCP, skills, plugins, configuration or instruction-file discovery.

Every one of those stays inside the provider. Uuldana's job ends at the process boundary.

## 2. Contract scope

The contract layer is written first and is tested on its own:

- `uuldana-core`: provider adapter contract, capability model, transports (as descriptions), normalized events, sessions and run lifecycle, permission policies, error classification, registry and discovery.
- `uuldana-provider-sdk`: stable re-export surface for adapter authors.

The runtime, the transports, the providers and the `uuldana` binary are built on these contracts (section 16). The contracts themselves do no I/O and start no process.

## 3. Components and boundaries

```
                    +--------------------------+
                    |  Terminal Entry Point    |  uuldana-cli (bin: uuldana)
                    +------------+-------------+
                                 |
                    +------------v-------------+
                    |      Core Supervisor     |  future: uuldana-runtime
                    |  (decides, never thinks) |
                    +--+------+-----+------+---+
                       |      |     |      |
        +--------------+  +---+--+ ++----+ +-------------------+
        |                 |      | |     |                     |
+-------v--------+ +------v----+ | | +---v--------------+ +----v-----------+
|Provider Registry| | Lifecycle | | | Permission       | | Failover State |
| (registry.rs)  | | + Events  | | | Contract         | | (future)       |
+-------+--------+ +-----------+ | | (permission.rs)  | +----------------+
        |                        | |
+-------v--------+        +------v-v-----+
| Provider SDK   |        | Session      |
| ProviderAdapter|        | Contract     |
| (adapter.rs)   |        | (session.rs) |
+-------+--------+        +--------------+
        | LaunchPlan + TransportSpec (data only)
+-------v-----------------------------------------------+
| Transports (future: uuldana-transport, runtime owned) |
|  Native PTY  |  ACP (JSON-RPC 2.0)  |  Custom CLI     |
+-------------------------------------------------------+
```

The guiding rule (ADR 0002): **the adapter describes, the transport moves bytes, the supervisor decides.**

| Component | Responsibility | Must not |
| --- | --- | --- |
| Entry point (`uuldana-cli`) | Parse arguments, wire registry and runtime, render output | Contain provider logic |
| Supervisor (runtime) | Drive lifecycle, own sessions and the event log, enforce policy, choose transport | Reason about code, call LLMs, edit provider config |
| Provider registry | Hold compiled-in adapters, check contract versions, run discovery | Know any provider by name |
| Provider adapter | Detect a provider, state capabilities, map permissions, produce launch and resume plans, decode output | Spawn processes, write files, read credentials |
| Transport | Spawn, connect, frame bytes or messages, resize, shut down | Interpret provider semantics |
| Session | Uuldana-owned record of runs and their native session references | Store provider transcripts or hidden state |
| Event | Provider-neutral vocabulary with fidelity labels | Pretend inferred data is native |
| Permission | Approval and sandbox policy, escalation checks, decision provenance | Grant anything automatically |

### Crate map

| Crate | Contents |
| --- | --- |
| `uuldana-core` | All contracts. No I/O, no async, no third-party dependencies. |
| `uuldana-provider-sdk` | Re-exports of the adapter-facing subset of `uuldana-core`. |
| `uuldana-cli` | The `uuldana` binary and its terminal frame. |
| `uuldana-runtime` | Supervisor, orchestrator, journal, configuration, limit detection, mock provider. |
| `uuldana-transport` | PTY, ACP and custom CLI transports. |
| `uuldana-providers` | Program discovery, profiles learned from `--help`, manifest adapters. |

## 4. Data ownership

| Data | Owner | Uuldana's access |
| --- | --- | --- |
| Provider configuration, settings, MCP config, skills, plugins, instruction files | Provider | None. Never read for modification, never written. |
| Provider authentication files and tokens | Provider | None. Not read, not copied. |
| Native transcripts and session stores | Provider | Referenced by `NativeSessionId` only. Optional read-only capture in later phases. |
| Hidden reasoning, model caches, native memory | Provider | None. Never claimed as transferable. |
| Session record (`Session`, `RunRecord`) | Uuldana | Full. |
| Normalized event log (`EventEnvelope`) | Uuldana | Full. Contains only what providers exposed. |
| Permission ceiling | User, stored by Uuldana | Read and enforced; never raised automatically. |

A `LaunchPlan` only describes the child's program, arguments, environment changes and working directory, and the core has no API that writes provider files. This is **not** a structural guarantee. The plan names an arbitrary executable that can write anywhere the operating system allows, and in-process adapters are ordinary Rust code. Leaving native configuration and authentication untouched is a responsibility of trusted adapters (`PROVIDER_SDK.md` rules 1 to 3), backed by review and conformance tests. The same holds for the workspace: the provider agent is expected to edit workspace files, and a `PermissionPolicy` describes what the provider is asked to enforce, not an operating-system sandbox imposed by Uuldana.

## 5. Provider discovery

1. The entry point builds a `ProviderRegistry` and registers compiled-in adapters (`register`). Registration rejects duplicate ids, adapters with no transport, and incompatible contract versions.
2. `ProviderRegistry::discover(&dyn HostProbe)` calls each adapter's `detect` and then `capabilities`.
3. `HostProbe` is implemented by the runtime. It is read-only by design: find an executable, run a short command such as `--version` with a timeout, read an environment variable, report the platform. On Windows, `find_executable` must honor `PATHEXT` because many CLIs are installed as `.cmd` shims.
4. The result is a `DiscoveryReport` per provider with either an `Installation` plus `CapabilitySet`, or a classified `AdapterError` (`NotInstalled`, `IncompatibleVersion`, ...).

Compiled-in adapters are registered statically. Programs found on the search path, and adapters described by a manifest, are added at start by `uuldana-providers` (section 16) without changing the trait.

## 6. Adapter lifecycle

```
register -> detect -> capabilities -> map_permissions -> validate_mapping (core)
         -> launch_plan | resume_plan -> LaunchPlan::validate (core)
         -> runtime spawns -> event_decoder.decode(frames...) -> shutdown_plan
         -> (optional) capture_session, prepare_handoff
```

Run state machine (`lifecycle.rs`):

| From | Transition | To |
| --- | --- | --- |
| `Pending` | `Launch` | `Launching` |
| `Launching` | `Started` | `Running` |
| `Launching`, `Running` | `StopRequested` | `Stopping` |
| any non-terminal state | `End(RunEnd)` | `Ended(RunEnd)` |

`RunEnd` is one of `Completed`, `Exited { code }`, `LimitReached(LimitKind)`, `Failed(ErrorClass)`, `Cancelled`, `Terminated`. `Ended` is terminal. Invalid transitions return `LifecycleError` and leave state unchanged. `Session` enforces at most one non-terminal run at a time.

### 6.1 Shutdown

`ShutdownPlan { steps, grace }` is executed by the runtime as follows (ADR 0008):

1. Run `steps` in order; each step is bounded by `grace`.
2. A step the transport reports as unsupported, or that fails, is recorded as a `Diagnostic` and its `ShutdownStep::fallback()` is tried once. `Interrupt` falls back to `CloseInput`; other steps have no fallback. Unsupported steps do not consume the grace period.
3. After the steps, wait up to `grace` for the direct child to exit: the run ends `Cancelled`.
4. Otherwise force-terminate: the run ends `Terminated`.

`Cancelled` and `Terminated` are deliberately distinct: the first means the provider shut down on request, the second means Uuldana had to kill it, so native state may not have been saved.

### 6.2 Run finalization

- A run is finalized when its **direct child process is reaped** (or the connection is closed), not when every output stream reaches end of file.
- After reaping, the transport performs bounded process-tree cleanup and output drain, then delivers exactly one `Exited` frame. Descendants retaining inherited output are a process-tree ownership concern (job objects, process groups); neither their cleanup nor waiting for stream EOF may keep a run alive indefinitely.
- Finalization always produces exactly one `End(RunEnd)` transition. Errors raised while finalizing are recorded as diagnostics and never trigger a second cleanup pass.

#### Windows teardown and reader progress

A native completion observer decides when a run's process tree is gone, because
traces showed that zero Job membership alone is not completion evidence. The
vendored `subc-jobobject` patch isolates the native code in that dependency;
workspace `unsafe_code = "forbid"` remains in force. See its
[audit and provenance](../vendor/subc-jobobject/UULDANA_PATCH.md).

The Job associates a completion port before assignment and retains owned root
process handles before resume. Before requesting termination, the transport
captures an observer with owned descendant synchronization/query handles.
Birth notifications and bounded process-list snapshots supply acquisition hints.
Successful snapshots use the documented returned prefix even when assigned and
returned counts differ during termination. Pinned identity/membership checks,
cumulative TotalProcesses accounting, zero active membership and signaling of
every owned process handle establish completion. A trusted birth PID that no
longer has a process object can be retired using the documented native absence
result; foreign/reused identities and access denial cannot fabricate completion.
Neither ACTIVE_PROCESS_ZERO, numeric PID disappearance nor pipe EOF proves it.

Notification loss falls back to periodic snapshots and handle polling. Missing
identity coverage, acquisition/query errors and unsignaled handles produce a
bounded diagnostic, even if the output streams reach EOF. Kill-on-close remains
the abandonment fallback. Each native handle has one RAII owner; the wait worker
polls outside the tree mutex. Direct-reap notification permits asynchronous
ConPTY close only after observer capture and the termination request.

Observation reserves 2,750 ms from the first successful termination request,
leaving the unchanged 250 ms D1 reader-progress allowance within the existing
three-second caller cleanup bound. Readers continue during native cleanup.
Only verified native completion starts the reader budget without a teardown
diagnostic. D1 still pauses for bounded-channel backpressure and never replenishes
spent reader time. EOF is separate reader evidence; a retained inherited handle
can still trigger a legitimate drain diagnostic after native completion.

HostProbe's whole-probe deadline starts before worker/runtime creation and
startup. The descendant regression supplies two seconds; failure cleanup has a
separate three-second bound. Caller deadlines remain authoritative, including
during native observation. HostProbe still rejects transport diagnostics as I/O
errors. Scheduling/backpressure prevent an independent hard real-time guarantee.

Launcher creation, Toolhelp thread discovery for pipes, original thread ownership
for PTYs, shutdown/cancellation policy and contract 0.2 are unchanged by the
observer, which adds native ownership and observation only.

### 6.3 Terminal precedence

The final `RunEnd` is computed by `RunOutcome` (`lifecycle.rs`), which every runtime must use. The runtime feeds it every normalized event (`observe`) and then calls `finalize(RunFinish)`:

| Priority | Condition | `RunEnd` |
| --- | --- | --- |
| 1 | `RunFinish::Aborted(class)`: supervisor-side failure (decoder panic, unrecoverable transport error) | `Failed(class)` |
| 2 | `RunFinish::Killed`: stop requested, provider force-terminated | `Terminated` |
| 3 | `RunFinish::Stopped`: stop requested, provider exited within the plan | `Cancelled` |
| 4 | Natural exit with a pending turn signal (`LimitReached`, non-protocol `ProviderError`, `TurnEnded(Cancelled)`) | that signal |
| 5 | Natural exit, last turn ended `EndTurn`, exit success | `Completed` |
| 6 | Natural exit otherwise | `Exited { code }` |

Signals are scoped to the current turn: `TurnStarted` and `TurnEnded(EndTurn)` clear earlier signals, so a provider that recovers from a rate limit or a transient error is not reported as failed. Within a turn the latest signal wins. An explicit user stop outranks provider signals, so a run cancelled by the user is never reported as failover eligible.

### 6.4 Terminal dimensions and platform validation

`TerminalSize::is_valid` is the portable rule: both dimensions must be non-zero. Backends have stricter limits that the core does not encode. ConPTY takes its size as a Win32 `COORD`, whose fields are signed 16-bit integers, so Windows accepts at most 32767 rows or columns. Such backend-specific limits are validated by the transport for the backend in use, before any native call, and reported as a typed transport error. Adapters only pick an initial size; they never need to know the backend limit.

## 7. Capability negotiation

Every capability is `Supported`, `Unsupported` or `Unknown` (ADR 0003). Absence means `Unknown`, and `Unknown` never passes `CapabilitySet::require`.

Statements carry evidence: `Declared` (adapter, static) < `Probed` (from the installed binary) < `Negotiated` (from the live connection, such as an ACP `initialize` response). `CapabilitySet::negotiate(declared, observed, transport)`:

1. starts from the declared set;
2. lets observed statements of equal or higher evidence replace declared ones;
3. applies transport rules last: on any transport other than `NativePty`, `NativeTerminalUi` is forced to `Unsupported`, because ACP and structured CLI modes carry data that Uuldana renders itself.

Third parties can add namespaced capabilities (`Capability::Custom("vendor.feature")`) without a core change.

## 8. Event normalization

Decoders (`EventDecoder`) turn `TransportFrame`s (`Output`, `Stderr`, `Message`, `Exited`) into `DecodedEvent`s. The supervisor wraps them into `EventEnvelope { seq, at, session, run, provider, fidelity, event }`.

`Fidelity` labels every event:

- `Native`: parsed from a structured, documented message (ACP `session/update`, a JSON lines record);
- `Inferred`: heuristic, typically matching terminal text in a PTY;
- `Synthetic`: produced by Uuldana (for example lifecycle changes).

The taxonomy (`AgentEvent`) covers lifecycle, native session identification, turns and stop reasons, visible message and thought chunks, tool calls, permission requests and resolutions, usage, limits, provider errors, process exit, diagnostics and namespaced extensions. `ThoughtChunk` only ever holds reasoning the provider chose to display.

For ACP, an adapter may return no decoder; the runtime then applies a generic ACP mapping, so ACP semantics are implemented once, not per provider.

### Malformed protocol data

Malformed provider data is never fatal by itself:

- A decoder that cannot parse a structured record emits `ProviderError { class: ProtocolViolation }` (or a `Diagnostic`) and continues with the next record. `RunOutcome` ignores `ProtocolViolation` when choosing the final `RunEnd`.
- Framing failures in the transport (a record that is not valid UTF-8, a record above the size limit) are reported for that record only. The transport resynchronizes at the next record boundary and keeps delivering later records. The supervisor records the failure as a synthetic `ProviderError { class: ProtocolViolation }` event and keeps the run alive.
- A run is aborted (`RunFinish::Aborted`) only when the transport itself can no longer deliver data (for example the read side fails with an I/O error) or a decoder panics.

## 9. Session ownership

A `Session` is Uuldana's unified history record for one piece of work in one absolute workspace. It contains ordered `RunRecord`s. Each run records provider, transport, the **effective** permissions it launched with, lifecycle state, timestamps and, once known, the provider's `NativeSessionId`.

How the native id becomes known is stated by the launch plan (`NativeSessionBinding`): pre-assigned by the adapter, discovered from events, or unavailable.

Native session ids are provider-owned and **may change during a run** (ADR 0009): a provider can reset its conversation, fork, or resume another session without restarting. Uuldana records every id in order as `NativeSessionEntry { id, link }`, where `NativeSessionLink` is `New`, `Resumed`, `Forked` or `Unspecified`. `RunRecord::native_session()` is the current id and `native_sessions()` the history; the entry before a `Forked` entry is its parent. Decoders emit `NativeSessionChanged { native, link }` when they know how sessions relate and `NativeSessionIdentified { native }` otherwise. No id change is an error: `Session::observe_native_session` returns `First`, `Unchanged` or `Changed { previous }`, and its only failure is an unknown run, so native session handling can never stop a run or start a cleanup loop.

Adapter callbacks about an existing run (`capture_session`, `prepare_handoff`) receive a `RunContext` from `Session::run_context`. It carries the Uuldana session id, the absolute workspace and the run, because identical run records can exist in different sessions and workspaces, and many providers file their sessions per project directory.

Resuming a provider's native session is a provider capability (`ResumeById`, `ResumeLatest`, `SessionReplay`), never an assumption. Many CLIs scope "resume latest" to the current directory, which is why the workspace is part of the session.

## 10. Permission policies

Approval and sandbox are separate dimensions (ADR 0004):

| Dimension | Values, strict to permissive |
| --- | --- |
| `ApprovalMode` | `AskEveryAction`, `AskForRisky`, `NeverAsk` |
| `FilesystemAccess` | `ReadOnly`, `WorkspaceWrite`, `Unrestricted` |
| `NetworkAccess` | `Denied`, `Allowed` |

Rules enforced in code:

1. **Session ceiling.** The user sets `Session::permission_ceiling`. `Session::begin_run` rejects any run whose requested policy exceeds it in any dimension (`ExceedsCeiling`).
2. **Honest mapping.** `ProviderAdapter::map_permissions` reports what the provider will really do. If a provider cannot enforce a restriction, the mapping states the permissive value, and `validate_mapping` rejects it (`WouldEscalate`). Being stricter than requested is allowed.
3. **Unforgeable validation.** `LaunchRequest` carries `ValidatedPermissions`, whose fields are private. It can only be produced by `validate_mapping`.
4. **No automatic allow.** `PermissionResolution::by_policy` refuses `AllowOnce` and `AllowAlways`. Only `by_user` can allow.
5. **No escalation path.** There is no API that raises a ceiling. Failover reuses the same ceiling, so switching providers can never silently elevate permissions. Approvals granted inside one provider (for example "allow always") are listed as `NonTransferable::ProviderApprovals`.

## 11. Error classification

`ErrorClass` is provider-neutral: `NotInstalled`, `IncompatibleVersion`, `AuthenticationRequired`, `Limit(LimitKind)`, `Transient`, `ProtocolViolation`, `Unsupported`, `PermissionDenied`, `PolicyViolation`, `ProcessFailed`, `Cancelled`, `Configuration`, `Internal`.

`ErrorClass::disposition()` maps each class to `Fatal`, `UserActionRequired`, `RetrySameProvider` or `FailoverEligible`. Only `Limit(UsageQuota)` is failover eligible. Rate limits and transient failures retry the same provider. Permission denials, policy violations, cancellations and user-set budgets are never failover eligible: switching provider must not become a way around a decision.

`AdapterError` carries a class, a message and optionally the capability involved. Optional adapter methods default to `AdapterError::unsupported(capability)`, so "not implemented" is always an explicit, typed answer.

## 12. Future automatic failover integration

The contracts already contain every hook failover needs; the policy engine itself is a later phase.

```
decoder emits LimitReached(UsageQuota)            (fidelity says how sure we are)
   -> supervisor ends run: RunEnd::LimitReached
   -> ErrorClass::Limit(UsageQuota).disposition() == FailoverEligible
   -> optional source adapter prepare_handoff()   (provider-specific extras)
   -> supervisor builds HandoffPackage from the normalized event log
        (visible transcript only; not_transferred always lists hidden
         reasoning, native memory, provider approvals, internal state,
         in-flight tool calls)
   -> pick next provider from registry where required capabilities are Supported
   -> map_permissions + validate_mapping against the SAME session ceiling
   -> begin_run on the same Session -> launch_plan with the handoff as input
```

Open questions for that phase are tracked in `PROTOCOL_COMPATIBILITY.md` (notably: ACP v1 has no standard quota signal, so limit detection over ACP is provider specific).

### Limits of native session extraction and handoff

- **Capture is best effort.** `capture_session` may find nothing. Provider session stores are provider-owned, usually undocumented, may change between versions, and are often keyed by directory. Adapters read them only read-only and must report `Unknown` rather than guess.
- **Ids are not stable identities.** A native id may rotate within a run and may not survive provider upgrades. Lineage (`Forked`, `Resumed`) is only as reliable as the provider's own reporting.
- **Only visible context moves.** A `HandoffPackage` carries the workspace, an objective, the visible transcript taken from normalized events and the source native session reference. Hidden reasoning, native memory, provider approvals, internal state and in-flight tool calls are always listed in `not_transferred`.
- **Fidelity travels with the data.** Transcript entries keep their `Fidelity`; inferred terminal text is not presented as an exact transcript.
- **No cross-provider resume.** A native session id is meaningful only to the provider that issued it. The next provider receives context, never the previous provider's session.

## 13. Extensibility and compatibility

- **Contract versioning.** `ProviderDescriptor::contract` is checked at registration. While the major is `0`, minors must match exactly. See ADR 0007.
- **Additive growth.** Public enums that will grow are `#[non_exhaustive]`. Structs adapters receive or build are `#[non_exhaustive]` with constructors, so new fields are not breaking.
- **Trait growth.** New adapter methods are added with default implementations that return `Unsupported`.
- **Custom capabilities and events.** `Capability::Custom` and `AgentEvent::Extension` carry vendor data without core changes. ACP `_meta` and underscore-prefixed methods map naturally onto extensions.
- **Transports.** A new transport family is a new `TransportKind` and `TransportSpec` variant; existing adapters are unaffected.
- **Serialization.** The core has no serde (ADR 0005). Persisted formats live in the runtime, each with an explicit schema version.

## 14. Security and reliability risks

| Risk | Mitigation in contracts | Remaining work |
| --- | --- | --- |
| Silent permission escalation during switching | Ceiling check, `ValidatedPermissions`, no escalation API, policy cannot auto-allow | Conformance tests that launch args match the effective policy |
| Adapter launch args are opaque to the core | Adapters are trusted compiled-in code; rule documented in the SDK guide | Per-adapter conformance tests (runtime phase) |
| Secrets leaking into logs | `EnvChange` Debug redacts values; adapters must not read credentials | Log review in runtime |
| Modifying provider config or auth | Core offers no write API and `HostProbe` has no write method, but launched executables and adapter code can still write (no structural guarantee) | Treat adapters as trusted code: review for config-dir environment overrides and writes; conformance tests |
| Descendant processes keep a run alive or survive a stop | Finalization defined on reaping the direct child, bounded drain (ADR 0008) | Runtime: job objects or process groups, regression tests with output-holding descendants |
| One malformed record or framing error ends a run | Malformed data is non-terminal; `RunOutcome` ignores `ProtocolViolation` | Runtime: resynchronize framing at the next record |
| Native session id changes treated as fatal | Ids may rotate; `observe_native_session` never fails on a change (ADR 0009) | Runtime: record changes as diagnostics only |
| Windows `.cmd` / `.bat` shims and argument injection | Absolute program paths, NUL checks | Runtime must rely on Rust std batch-file escaping and handle `.cmd` resolution explicitly |
| ConPTY deadlocks on close | Out of core scope | Runtime: separate reader and writer threads, drain output on close |
| Probing triggers provider side effects (auto-update) | Detection limited to short commands with timeouts | Adapter review |
| Heuristic limit detection gives false positives | `Fidelity::Inferred` label | Failover policy must weigh fidelity |
| Hidden state assumed transferable | `HandoffPackage::not_transferred` always populated | None |
| Decoder panics take down the supervisor | Contract forbids panics on malformed input | Runtime should isolate decoders (`catch_unwind`) and fuzz them |
| Workspace glob membership (`crates/*`) | Intentional: a broken crate fails the build early | Coordination between workstreams |

## 15. Validation

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace --locked
```

Tested locally on Windows 11 with Rust 1.96.0 (`stable-x86_64-pc-windows-gnu`). No minimum supported Rust version is declared yet; the workspace uses edition 2024. GNU builds require complete native MinGW-w64 binutils (`dlltool` and an assembler on `PATH`); MSVC builds require Visual Studio Build Tools and a Windows SDK. See [Windows development](WINDOWS_DEVELOPMENT.md) for the reproducible prerequisites, pinned per-user GNU installation, and standard Cargo verification. `scripts/verify-windows.ps1` runs the ordinary Cargo commands without compiler flag overrides. CI configures Windows GNU/MSVC and Linux/macOS. Only Windows has been run by hand; no Linux or macOS machine has run the tests yet.

## 16. Implementation on top of the contracts

Contract 0.2 and `uuldana-core` are unchanged by the layers below.

| Area | Where | What it does |
| --- | --- | --- |
| ACP | `uuldana-transport` (pipe launch for `TransportSpec::Acp`), `uuldana-runtime::acp` | Newline delimited JSON-RPC over the child's standard streams. `AcpClient` builds client messages and answers permission requests; `AcpDecoder` is the single generic ACP to `AgentEvent` mapping used when an adapter has no decoder. Automatic rules can only reject a permission request, never allow it. |
| Native terminal | `uuldana-runtime::relay`, `uuldana-cli::terminal` | Relays the user's keyboard and screen to the provider's PTY, follows terminal resizes, answers ConPTY's own cursor query so the PTY does not wait on a reply that would otherwise be lost, and can stop a run gracefully on request. |
| Providers | `uuldana-providers` | No agent is known by name. `discover` finds console programs on the search path, `learned` asks each for `--help` once (cached), `help` and `learn` turn that text into a profile (how to start with a prompt, resume, and set permissions), and the profile becomes a manifest adapter (`providers::ExternalAdapter`). Manifests written by the user are loaded the same way and win for the same id. Permission mappings never claim a restriction the program's flags do not enforce. |
| Configuration | `uuldana-runtime::config` | User-defined provider and account order, permission ceiling, cooldowns, failover switch. No built-in priority. |
| Sessions | `uuldana-runtime::{journal, codec}` | Append-only JSON Lines journal per session with a versioned header, crash-tolerant replay that rebuilds `Session` and the event log, a count based retention limit. |
| Quota and cooldown | `uuldana-runtime::{universal_limit, cooldown}` | Usage-limit wording in terminal output becomes an inferred `LimitReached(UsageQuota)` event. Resting entries are tracked per configured entry and persisted. |
| Failover | `uuldana-runtime::{orchestrator, handoff}` | Picks entries by the user's order, skips resting or refused ones, rests an exhausted entry, builds a visible-only handoff (`HandoffPackage`) from the text the previous agent's screen showed (kept in the journal when the run ends) and starts the next entry with it, inside the same permission ceiling. |

No agent's own session files are read: they are provider-owned and undocumented. A handoff and a resume use what Uuldana itself saw on the screen, and the agent's own flag for continuing the latest conversation of the folder.

What is still not guaranteed: a limit message that a provider changes, a program whose help does not describe what it does, and any behavior on Linux or macOS beyond compilation. See the README for what was run against real programs.
