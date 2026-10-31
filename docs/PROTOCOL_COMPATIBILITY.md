# Protocol and Platform Compatibility

This document separates **confirmed behavior** (read in official documentation, with source and date) from **design assumptions** (decisions Uuldana makes that are not guaranteed by any upstream) and **unknowns** (questions that must be answered by experiments before the related feature ships).

All sources were checked on **2026-10-12**. Provider names appear here only as evidence. They never appear in `uuldana-core`.

## 1. Agent Client Protocol (ACP)

Sources:
- Overview: https://agentclientprotocol.com/protocol/overview
- Initialization: https://agentclientprotocol.com/protocol/initialization
- Session setup: https://agentclientprotocol.com/protocol/session-setup
- Prompt turn: https://agentclientprotocol.com/protocol/prompt-turn
- Tool calls and permissions: https://agentclientprotocol.com/protocol/tool-calls
- Schema: https://agentclientprotocol.com/protocol/schema
- Repository: https://github.com/agentclientprotocol/agent-client-protocol

### Confirmed

- ACP uses JSON-RPC 2.0 requests and notifications. Agents typically run as subprocesses of the client. Notifications never receive a response.
- Roles: the **Agent** runs the model and tools; the **Client** provides the user interface, manages the environment and controls access to resources. Uuldana acts as an ACP Client.
- `protocolVersion` is a single integer (uint16) identifying a MAJOR version, bumped only for breaking changes. The current stable version is `1`; the repository also contains a `schema/v2` directory.
- Version negotiation: the client sends the latest version it supports in `initialize`. If the agent supports it, it answers with the same version; otherwise it answers with the latest version it supports. If the client does not support that answer, it SHOULD close the connection and inform the user.
- Client capabilities include `fs.readTextFile`, `fs.writeTextFile`, `terminal` and session-related options. Agent capabilities include `loadSession`, `promptCapabilities` (`image`, `audio`, `embeddedContext`), `mcpCapabilities` (`http`, `sse`, the latter deprecated) and `sessionCapabilities` (`list`, `resume`, `close`, `delete`, `additionalDirectories`).
- Every agent MUST support `session/new`, `session/prompt`, `session/cancel` and `session/update`. `session/load` requires `loadSession` and replays history through `session/update`. `session/resume` requires `sessionCapabilities.resume` and does not replay history. `session/list` and `session/close` are gated by their capabilities.
- `session/new` takes an absolute `cwd` and a list of MCP servers (stdio always supported; http and sse gated by capabilities).
- `session/update` variants include `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `current_mode_update`, `config_option_update`, `session_info_update` and `usage_update`, plus capability-gated `notice`, `compaction_update` and `compaction_summary_chunk`.
- Stop reasons: `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled`.
- `session/cancel` is a notification. The client must answer pending `session/request_permission` requests with the `cancelled` outcome; the agent must end the turn with stop reason `cancelled`.
- `session/request_permission` offers options of kind `allow_once`, `allow_always`, `reject_once`, `reject_always`. The response outcome is `selected` (with an `optionId`) or `cancelled`.
- Tool kinds: `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, `switch_mode`, `other`. Tool statuses: `pending`, `in_progress`, `completed`, `failed`.
- `usage_update` reports context window `size` and `used` tokens and an optional cumulative `cost`.
- Extensibility: `_meta` fields on any type, custom methods prefixed with `_`, and custom capabilities advertised during initialization.
- The protocol does not define how the client renders the conversation. `terminal/*` methods are about running commands for the agent, not about showing a terminal UI.
- Official SDKs exist for Rust (`agent-client-protocol`, `agent-client-protocol-schema`), TypeScript, Python, Java and Kotlin.

### Design assumptions

- Uuldana is always the ACP **Client**. It never implements an ACP Agent.
- ACP never provides the provider's native terminal UI. `CapabilitySet::negotiate` forces `NativeTerminalUi = Unsupported` for every non-PTY transport.
- Uuldana offers its highest supported version and accepts any answer in its supported range (`negotiate_acp_version`). Uuldana currently supports `1..=1`.
- ACP capability flags map to Uuldana capabilities with `Evidence::Negotiated`: `loadSession` to `SessionReplay`, `sessionCapabilities.resume` to `ResumeById`, `sessionCapabilities.list` to `SessionList`. Cancellation and permission requests are part of the baseline and map to `Cancellation` and `PermissionRequests`.
- ACP permission option kinds map one to one onto `PermissionDecision`; a `cancelled` outcome maps to `PermissionDecision::Cancelled`.
- The generic ACP-to-event mapping lives once in the transport layer, not in each adapter.

### Unknowns

- ACP v1 has **no standard usage-limit or quota signal**. `usage_update` is about the context window. How each agent reports an exhausted quota over ACP (JSON-RPC error codes, `refusal`, `_meta`, notices) must be determined per agent, and such signals will usually be `Fidelity::Inferred`.
- What changes in ACP v2 and when agents adopt it.
- Whether a given agent's ACP mode loads the same user configuration, MCP servers, skills and instruction files as its interactive mode. This must be verified per agent before ACP is used as a primary transport.
- Whether ACP session ids are the same identifiers as the agent's interactive-mode session ids (affects cross-transport resume).

## 2. Native CLI lifecycle and resume

These facts justify the shape of the contracts (`ResumeById`, `ResumeLatest`, `PreassignSessionId`, workspace scoping). They are **not** encoded in `uuldana-core`.

Sources:
- Claude Code CLI reference: https://code.claude.com/docs/en/cli-reference
- Codex CLI reference: https://learn.chatgpt.com/docs/developer-commands?surface=cli (redirect target of https://developers.openai.com/codex/cli/reference)
- Gemini CLI session management: https://geminicli.com/docs/cli/session-management/ and CLI reference https://geminicli.com/docs/cli/cli-reference

### Confirmed

- **Claude Code**: `--continue` loads the most recent conversation in the current directory. `--resume` takes a session id or name (or opens a picker). `--session-id <uuid>` sets the session id at launch. `--fork-session` creates a new id when resuming. Non-interactive mode `-p` supports `--output-format stream-json`. `--permission-mode` accepts several modes, and `--no-session-persistence` disables resumability in print mode.
- **Codex CLI**: `codex resume [SESSION_ID]` with `--last` (scoped to the current directory unless `--all`). `codex exec` is the non-interactive mode, `codex exec resume` continues a run, and `--json` prints newline-delimited JSON events. `--sandbox` accepts `read-only`, `workspace-write`, `danger-full-access`; `--ask-for-approval` accepts `on-request` and `never`. `--ephemeral` disables persisted session files. `codex app-server` is marked experimental.
- **Gemini CLI**: `--resume` accepts `latest`, an index or a session UUID; `--list-sessions` lists sessions for the current project.

### Design assumptions

- "Resume latest" is commonly scoped to the working directory, so a Uuldana `Session` is bound to one absolute workspace.
- Approval and sandbox are separate concepts in at least one major CLI, which supports modeling them separately.
- Some CLIs accept a caller-chosen session id; adapters express this with `NativeSessionBinding::Preassigned` and `Capability::PreassignSessionId`.
- Some CLIs can disable session persistence; such launches must report `ResumeById` as `Unsupported`.

### Unknowns

- Gemini CLI ACP entry flag: the CLI reference lists `--experimental-acp`, while the ACP mode page describes `--acp`. Verify against the installed version.
- Kiro CLI and OpenCode: lifecycle, resume and structured modes have not been verified.
- Whether resuming a native session through a PTY reproduces the exact UI state (scrollback, pending prompts). Assume it does not.
- Whether native session ids are stable across provider upgrades.

## 3. Pseudo terminals

Sources:
- Microsoft, "Creating a Pseudoconsole session": https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session
- `portable-pty` documentation: https://docs.rs/portable-pty/latest/portable_pty/

### Confirmed

- Windows hosts pseudo terminals with ConPTY (`CreatePseudoConsole`). The host creates pipes, creates the pseudoconsole with an initial size, and starts the child with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `EXTENDED_STARTUPINFO_PRESENT`.
- Microsoft recommends servicing each channel on a separate thread; servicing everything on one thread can deadlock.
- `ResizePseudoConsole` changes the buffer size at runtime.
- `ClosePseudoConsole` terminates attached clients (and their process tree) and may emit a final frame that must be drained, otherwise teardown can deadlock.
- Closing the pseudoconsole while the child is still starting can make the child show an initialization error (`0xc0000142`).
- `portable-pty` (version 0.9.0 at the time of writing) exposes a synchronous API: `native_pty_system`, `PtySystem`, `PtyPair`, `MasterPty`, `SlavePty`, `Child`, `CommandBuilder`, `PtySize`.
- `CreatePseudoConsole` takes its initial size as a `COORD` (https://learn.microsoft.com/en-us/windows/console/createpseudoconsole), and `COORD` has signed 16-bit `X` and `Y` fields (`windows-sys` 0.61.2 Win32 bindings: `pub struct COORD { pub X: i16, pub Y: i16 }`). ConPTY therefore cannot represent more than 32767 rows or columns. `ResizePseudoConsole` uses the same type.
- `CreatePseudoConsole` requires Windows 10 version 1809 or Windows Server 2019.

### Design assumptions

- PTY transport passes bytes through unchanged to the user's terminal; normalization from PTY output is heuristic (`Fidelity::Inferred`).
- `TransportSpec::NativePty` carries only the initial size; resize is a runtime operation.
- `TerminalSize::is_valid` stays backend-neutral (non-zero dimensions). Backend limits such as the ConPTY `COORD` range are checked by the transport for the backend in use.
- `ShutdownStep::Interrupt` means writing the interrupt character on a PTY. On a plain pipe the transport may report it as unsupported; the runtime then falls back to `CloseInput` (ADR 0008). Force termination after the grace period is the runtime's job.
- A run is finalized when the direct child is reaped; descendants that keep a PTY or pipe open must not delay finalization (ADR 0008).

### Unknowns (owned by the PTY feasibility work)

- Whether ConPTY output is byte-identical to what the provider wrote. ConPTY renders through its own buffer, so escape sequences may be rewritten; this affects "native UI fidelity" claims on Windows.
- Behavior of mouse input, bracketed paste, alternate screen and true color through ConPTY with each provider.
- Exact interrupt semantics on Windows children that are `.cmd` shims (`cmd.exe` "Terminate batch job" prompts).

## 4. Rust async and process management

Sources:
- Rust project goals 2026, async fn dynamic dispatch: https://rust-lang.github.io/goals/2026/roadmap-just-add-async.html
- `async-trait` crate: https://docs.rs/async-trait
- `std::process::Command`: https://doc.rust-lang.org/std/process/struct.Command.html

### Confirmed

- `async fn` in traits has been stable since Rust 1.75, but traits with async methods are not dyn-compatible. Native dynamic dispatch for async trait methods is a 2026 to 2027 project goal, not a stable feature.
- `async-trait` works around this by boxing every future.
- On Windows, `Command` resolves the executable itself. Without an extension only `.exe` is found; other extensions such as `.cmd` and `.bat` must be given explicitly.
- `Command` documents that `cmd.exe` and `.bat` files decode arguments in a non-standard way and are vulnerable to malicious arguments.
- Child processes inherit the parent environment by default. Environment variable names are case-insensitive on Windows and case-sensitive elsewhere. On Windows, clearing `PATH` for the child still searches the parent's `PATH`.

### Design assumptions

- `uuldana-core` traits are synchronous and I/O-free (ADR 0001); the runtime owns async execution and can call adapters from blocking tasks where needed.
- Adapters return absolute program paths. Resolving `.cmd` shims and quoting their arguments safely is a runtime responsibility.
- `EnvChange` keys are treated as opaque `OsString`s; the runtime must apply Windows case-insensitivity when merging.

### Unknowns

- Whether the runtime should run detection on a blocking thread pool or a dedicated thread; decided by the runtime implementation.

## 5. Toolchain used for validation

- Windows 11 Pro, Rust 1.96.0, default toolchain `stable-x86_64-pc-windows-gnu` (self-contained MinGW linker). The MSVC toolchain is installed but no Visual Studio build tools are present, so MSVC linking is not available on this machine.
- `cargo` is not on the default `PATH` of this machine; commands were run after prepending `%USERPROFILE%\.cargo\bin`.
