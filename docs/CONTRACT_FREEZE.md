# Contract Freeze

- Contract version: **0.2** (`uuldana_core::CONTRACT_VERSION`)
- Consumers: `uuldana-runtime`, `uuldana-transport`, `uuldana-providers`, third-party adapters

The interfaces below are frozen. Changing any of them requires: a written reason in the change log at the end of this file, a contract version bump (ADR 0007), and checking every dependent implementation in the workspace.

## 1. What to import

| You are writing | Depend on | Import |
| --- | --- | --- |
| A provider adapter, including the mock provider | `uuldana-provider-sdk` | `uuldana_provider_sdk::prelude::*` plus specific items |
| Runtime, supervisor, transports | `uuldana-core` | `uuldana_core::{...}` (root re-exports) |

Workspace dependency lines:

```toml
uuldana-core.workspace = true
uuldana-provider-sdk.workspace = true
```

Source files defining the contracts (all under `crates/uuldana-core/src/`):

| File | Defines |
| --- | --- |
| `adapter.rs` | `ProviderAdapter`, `ProviderDescriptor`, `HostProbe`, `ProbeOutput`, `ProbeError`, `Platform`, `Installation`, `LaunchRequest`, `ResumeRequest`, `LaunchPlan`, `LaunchPlanError`, `EnvChange`, `NativeSessionBinding`, `ShutdownPlan`, `ShutdownStep`, `EventDecoder` |
| `transport.rs` | `TransportKind`, `TransportSpec`, `TerminalSize`, `AcpVersionRange`, `negotiate_acp_version`, `StreamFraming`, `StdinMode`, `TransportFrame`, `ExitInfo`, `TransportError` |
| `event.rs` | `AgentEvent`, `DecodedEvent`, `EventEnvelope`, `Fidelity`, `Role`, `StopReason`, `ToolKind`, `ToolStatus`, `LimitKind`, `LimitSignal` |
| `capability.rs` | `Capability`, `CustomCapability`, `CapabilitySet`, `CapabilityEntry`, `Support`, `Evidence`, `CapabilityError` |
| `permission.rs` | `PermissionPolicy`, `ApprovalMode`, `SandboxPolicy`, `FilesystemAccess`, `NetworkAccess`, `PermissionDimension`, `PermissionMapping`, `ValidatedPermissions`, `validate_mapping`, `PermissionDecision`, `PermissionResolution`, `DecisionSource`, `PermissionError` |
| `session.rs` | `Session`, `RunRecord`, `NewRun`, `RunContext`, `NativeSessionRef`, `NativeSessionLink`, `NativeSessionEntry`, `NativeSessionUpdate`, `SessionError` |
| `lifecycle.rs` | `RunState`, `RunTransition`, `RunEnd`, `RunFinish`, `RunOutcome`, `LifecycleError` |
| `error.rs` | `ErrorClass`, `Disposition`, `AdapterError` |
| `registry.rs` | `ProviderRegistry`, `DiscoveryReport`, `Discovered`, `RegistryError` |
| `handoff.rs` | `HandoffPackage`, `TranscriptEntry`, `NonTransferable`, `SessionCapture` |
| `ids.rs` | `ProviderId`, `SessionId`, `RunId`, `NativeSessionId`, `ToolCallId`, `PermissionRequestId`, `IdError` |
| `time.rs` | `Timestamp` |
| `version.rs` | `ContractVersion`, `CONTRACT_VERSION` |

## 2. Public interfaces

### `ProviderAdapter` (Send + Sync + 'static, all methods synchronous)

```rust
fn descriptor(&self) -> &ProviderDescriptor;
fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError>;
fn capabilities(&self, installation: &Installation) -> CapabilitySet;
fn map_permissions(&self, installation: &Installation, requested: &PermissionPolicy) -> PermissionMapping;
fn launch_plan(&self, installation: &Installation, request: &LaunchRequest) -> Result<LaunchPlan, AdapterError>;
fn resume_plan(&self, installation: &Installation, request: &ResumeRequest) -> Result<LaunchPlan, AdapterError>;   // default: Unsupported(ResumeById)
fn event_decoder(&self, transport: TransportKind) -> Option<Box<dyn EventDecoder>>;
fn capture_session(&self, context: &RunContext<'_>, probe: &dyn HostProbe) -> Result<SessionCapture, AdapterError>; // default: Unsupported(SessionCapture)
fn prepare_handoff(&self, context: &RunContext<'_>) -> Result<HandoffPackage, AdapterError>;                       // default: Unsupported(HandoffCapture)
fn shutdown_plan(&self, transport: TransportKind) -> ShutdownPlan;                                                 // default: [Interrupt], 5 s grace
```

### `HostProbe` (implemented by the runtime; Send + Sync; read-only)

```rust
fn find_executable(&self, name: &str) -> Option<PathBuf>;   // must honor PATHEXT on Windows
fn run(&self, program: &Path, args: &[&str], timeout: Duration) -> Result<ProbeOutput, ProbeError>;
fn env_var(&self, name: &str) -> Option<OsString>;
fn platform(&self) -> Platform;
```

### `EventDecoder` (Send)

```rust
fn decode(&mut self, frame: TransportFrame<'_>, out: &mut Vec<DecodedEvent>);
```

Runtime obligations: deliver frames in order; deliver `TransportFrame::Exited` exactly once, last; for `CustomCli` with `StreamFraming::Lines` and for ACP, deliver one complete record per `Message`; never call `decode` concurrently on the same decoder.

### Session and lifecycle API (contract 0.2 additions marked *)

```rust
impl Session {
    pub fn run_context(&self, id: &RunId) -> Option<RunContext<'_>>;                                 // *
    pub fn observe_native_session(&mut self, id: &RunId, native: NativeSessionId,
                                  link: NativeSessionLink) -> Result<NativeSessionUpdate, SessionError>; // *
    pub fn record_native_session(&mut self, id: &RunId, native: NativeSessionId) -> Result<(), SessionError>; // semantics changed
}
impl<'a> RunContext<'a> {                                                                             // *
    pub fn session_id(&self) -> &'a SessionId;
    pub fn workspace(&self) -> &'a Path;
    pub fn run(&self) -> &'a RunRecord;
    pub fn native_session(&self) -> Option<NativeSessionRef>;
}
impl RunRecord {
    pub fn native_session(&self) -> Option<&NativeSessionId>;   // now: current (latest) id
    pub fn native_ref(&self) -> Option<NativeSessionRef>;        // now: current (latest) id
    pub fn native_sessions(&self) -> &[NativeSessionEntry];     // *
}
pub enum NativeSessionLink { New, Resumed, Forked, Unspecified }                 // * non_exhaustive
pub struct NativeSessionEntry { pub id: NativeSessionId, pub link: NativeSessionLink } // *
pub enum NativeSessionUpdate { First, Unchanged, Changed { previous: NativeSessionId } } // *

pub enum RunEnd { Completed, Exited { code }, LimitReached(LimitKind), Failed(ErrorClass),
                  Cancelled, Terminated /* * */ }                                  // non_exhaustive
pub enum RunFinish { Exited(ExitInfo), Stopped, Killed, Aborted(ErrorClass) }    // * non_exhaustive
impl RunOutcome {                                                                 // * Default + Clone
    pub fn new() -> Self;
    pub fn observe(&mut self, event: &AgentEvent);
    pub fn pending_signal(&self) -> Option<RunEnd>;
    pub fn finalize(&self, finish: RunFinish) -> RunEnd;
}

impl ShutdownStep { pub fn fallback(&self) -> Option<ShutdownStep>; }            // *
impl Timestamp { pub fn from_system_time(time: SystemTime) -> Self; }            // *  (Timestamp::now removed)
impl SessionCapture { pub fn with_parent(self, parent: NativeSessionId) -> Self; } // * plus field `parent`
impl HandoffPackage { pub fn for_run(context: &RunContext<'_>) -> Self; }         // * plus field `source`
// AgentEvent::NativeSessionChanged { native: NativeSessionId, link: NativeSessionLink }  *
```

### Core functions the supervisor must call

| Function | When | Failure means |
| --- | --- | --- |
| `validate_mapping(&requested, &mapping)` | After `map_permissions`, before building a `LaunchRequest` | Provider would run with more permission than requested. Do not launch. |
| `LaunchPlan::validate(&request)` | After `launch_plan` / `resume_plan` (use `request.launch` for resume), before spawning | Adapter bug. Do not spawn. |
| `Session::begin_run(NewRun)` | Before spawning | Another run active, duplicate id, or permissions above ceiling. |
| `Session::transition_run(&id, transition, at)` | On every lifecycle change | Invalid transition; state unchanged. |
| `Session::observe_native_session(&id, native, link)` | On `NativeSessionIdentified` (`Unspecified`), `NativeSessionChanged` (its link), a pre-assigned id (`New`) or a resumed id at start (`Resumed`) | Only an unknown run. Never stop a run because of this call. |
| `RunOutcome::observe(&event)` / `finalize(finish)` | Every normalized event / exactly once at finalization | n/a (pure) |
| `ShutdownStep::fallback()` | When a shutdown step is unsupported or fails | n/a |
| `Session::run_context(&id)` | Before `capture_session` / `prepare_handoff` | `None` for an unknown run. |
| `CapabilitySet::negotiate(&declared, &observed, transport)` | After connecting (ACP `initialize`, probes) | n/a (always returns a set) |
| `negotiate_acp_version(range, agent_response)` | On ACP `initialize` response | Close the connection and inform the user. |
| `ProviderRegistry::register(adapter)` | At startup | Duplicate id, no transports, or incompatible contract. |

## 3. Important types and invariants

- `ValidatedPermissions` has private fields; only `validate_mapping` creates it. Its `effective()` is never more permissive than `requested()` in any dimension.
- `Session` invariants: absolute workspace; at most one non-terminal run; each run's requested policy within `permission_ceiling`; run state changes only through `RunState::apply`. `RunRecord::permissions()` stores the **effective** policy.
- Native session ids may rotate within a run. Every different id is appended to `native_sessions()` and becomes current; repeating the current id records nothing. Changes are never errors and ended runs accept observations (ADR 0009).
- `RunContext` can only be obtained from `Session::run_context`, so its workspace and session id always match the run.
- `RunState` transitions: `Pending` to `Launching` on `Launch`; `Launching` to `Running` on `Started`; `Launching` or `Running` to `Stopping` on `StopRequested`; any non-terminal state to `Ended` on `End(RunEnd)`. `Ended` is terminal.
- `RunOutcome` precedence: `Aborted(c)` gives `Failed(c)` > `Killed` gives `Terminated` > `Stopped` gives `Cancelled` > pending turn signal > `Completed` (last turn `EndTurn` and exit success) > `Exited { code }`. Signals are cleared by `TurnStarted` and `TurnEnded(EndTurn)`; the latest signal in a turn wins; `ProviderError { class: ProtocolViolation }` is never a signal (ADR 0008).
- `CapabilitySet`: absent means `Unknown`; `require` passes only for `Supported`; refinement uses `Evidence` rank (`Declared` < `Probed` < `Negotiated`, equal rank replaces); `negotiate` forces `NativeTerminalUi = Unsupported` on non-PTY transports.
- `PermissionResolution::by_policy` rejects `AllowOnce` and `AllowAlways`.
- `HandoffPackage::not_transferred()` always equals `NonTransferable::ALL`.
- `EnvChange`'s `Debug` output redacts values.
- `TerminalSize::is_valid` only requires non-zero dimensions. Backend limits (ConPTY: at most 32767) are checked by the transport.
- `ProviderId`: `[a-z0-9][a-z0-9._-]{0,63}` without `..`. Other ids are opaque, non-empty, at most 512 bytes, no control characters.
- The core never reads the system clock; timestamps come from the runtime.

## 4. Error semantics

- `AdapterError { class: ErrorClass, message, capability: Option<Capability> }`. Optional methods that are not implemented return `ErrorClass::Unsupported` with the capability set.
- `ProbeError` converts into `AdapterError`: `NotFound -> NotInstalled`, `TimedOut -> Transient`, `Denied -> PermissionDenied`, `Io -> Internal`.
- `ErrorClass::disposition()`:

| Class | Disposition |
| --- | --- |
| `Limit(UsageQuota)` | `FailoverEligible` (the only one) |
| `Limit(RateLimit)`, `Transient` | `RetrySameProvider` |
| `Limit(ContextWindow)`, `Limit(Budget)`, `Limit(Unknown)`, `NotInstalled`, `IncompatibleVersion`, `AuthenticationRequired`, `PermissionDenied`, `ProcessFailed`, `Configuration` | `UserActionRequired` |
| `ProtocolViolation`, `Unsupported`, `PolicyViolation`, `Cancelled`, `Internal` | `Fatal` |

- Dispositions are advice from the contract. The orchestrator in `uuldana-runtime` is what acts on `FailoverEligible`.
- Malformed provider data is non-terminal: decoders emit `ProviderError { class: ProtocolViolation }` or `Diagnostic`; per-record framing errors are recorded the same way and the transport resynchronizes. Only an unusable transport or a decoder panic aborts a run (`RunFinish::Aborted`).
- `SessionError::NativeSessionConflict` no longer exists. Native session calls fail only for an unknown run.
- Errors raised during finalization are diagnostics; finalization produces exactly one `End(RunEnd)` and never starts a second cleanup.
- Error enums are `#[non_exhaustive]`; match with a wildcard arm.

## 5. Compatibility assumptions

- Rust edition 2024, tested with Rust 1.96.0 on Windows (GNU toolchain). No MSRV is declared. With the runtime dependencies, GNU builds need `dlltool` and an assembler on `PATH` (see `ARCHITECTURE.md` section 15).
- `uuldana-core` has no third-party dependencies, no `unsafe` code and no host I/O.
- ACP: Uuldana is the Client; supported protocol versions `1..=1`; ACP never implies native terminal UI.
- `LaunchPlan.program` is absolute. Resolving `.cmd` / `.bat` shims on Windows and quoting their arguments safely is the runtime's job.
- `LaunchPlan.cwd` equals `LaunchRequest.workspace`.
- `LaunchPlan.env` changes apply on top of the inherited environment. On Windows the runtime must treat keys case-insensitively when merging.
- `Timestamp` is milliseconds since the Unix epoch.
- No serialization format is frozen. Do not persist these types with ad-hoc formats; the runtime keeps its own versioned formats.
- Adapters are trusted code. The contract keeps file-writing APIs out of the core but cannot prevent a launched executable from writing (ADR 0002 amendment).

## 6. Breaking changes in 0.2

| Change | Kind | Affected in workspace at freeze time |
| --- | --- | --- |
| `ProviderAdapter::capture_session` takes `&RunContext<'_>` instead of `&RunRecord` | Signature | No overrides or callers |
| `ProviderAdapter::prepare_handoff` takes `&RunContext<'_>` instead of `&RunRecord` | Signature | No overrides or callers |
| `Timestamp::now()` removed | Removal | No callers (runtime has its own clock helper) |
| `SessionError::NativeSessionConflict` removed | Removal | Not matched anywhere |
| `record_native_session` no longer fails on a different id; `RunRecord::native_session()` returns the latest id | Semantics | Runtime calls it; behavior only becomes more permissive |
| `RunEnd::Terminated` added | Additive (non_exhaustive) | Runtime compiles; its forced-stop test still expects `Cancelled` until migrated |
| `AgentEvent::NativeSessionChanged` added | Additive (non_exhaustive) | Runtime matches with a wildcard |
| `SessionCapture::parent`, `HandoffPackage::source` fields | Additive (non_exhaustive / private-field structs) | None |
| `CONTRACT_VERSION` 0.1 to 0.2 | Version | In-tree adapters pick it up through `ProviderDescriptor::new` |

`uuldana-runtime` and `uuldana-transport` compile and pass their tests unchanged against 0.2.

## 7. Change log

| Date | Version | Change | Reason | Dependents checked |
| --- | --- | --- | --- | --- |
| 2026-10-12 | 0.1 | Initial freeze | First contracts | `uuldana-provider-sdk`, `uuldana-cli` |
| 2026-10-12 | 0.2 | `RunContext` for `capture_session` / `prepare_handoff`; native session history with `NativeSessionLink`, `observe_native_session`, `AgentEvent::NativeSessionChanged`, removal of `NativeSessionConflict`; `RunEnd::Terminated`, `RunFinish`, `RunOutcome`; `ShutdownStep::fallback`; `Timestamp::now` removed, `Timestamp::from_system_time` added; `SessionCapture::parent`, `HandoffPackage::source` / `for_run` | Lifecycle review remediation (ADR 0008, ADR 0009, amendments to ADR 0001, 0002, 0006) | `uuldana-provider-sdk`, `uuldana-cli`, `uuldana-runtime`, `uuldana-transport` (compile and tests, unchanged) |
