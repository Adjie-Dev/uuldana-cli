# Uuldana PTY feasibility findings

> Historical baseline under contract 0.1. Fixtures now live under `crates/uuldana-transport/examples/`; [Windows development](WINDOWS_DEVELOPMENT.md) supersedes the earlier assembler workaround.

Research and local execution: 2026-10-12. Contract: frozen 0.1. Tested host: Windows 11 Pro, build 26300, x86_64; Rust/Cargo 1.96.0, `x86_64-pc-windows-gnu`. Linux and macOS have **not** been executed. CI configuration is not execution evidence.

## Library comparison

| Library / documentation version | Relevant interface | Windows assessment | Decision |
| --- | --- | --- | --- |
| [portable-pty 0.9.0](https://docs.rs/portable-pty/0.9.0/portable_pty/) | Platform-selected PTY, separate reader/writer, resize, argv builder, child wait/killer | ConPTY backend inspected and exercised locally; blocking interfaces require workers | Selected for this isolated prototype; keep behind transport boundary |
| [pty-process 0.5.3](https://docs.rs/pty-process/latest/pty_process/) | std/Tokio subprocess wrappers; async read/write and resize | [Current implementation](https://github.com/doy/pty-process/blob/main/src/sys.rs) imports Unix descriptors and termios; no ConPTY implementation found | Useful Unix option, does not satisfy this Windows prototype alone |
| [conpty 0.7.0](https://docs.rs/conpty/0.7.0/conpty/struct.Process.html) | Windows process input/output, resize, timed wait and exit | Windows-specific alternative; documentation reviewed, not executed | Would need a separate Unix backend |
| [expectrl 0.9.0](https://docs.rs/expectrl/latest/expectrl/) | Expect-style matching, interaction, optional async API | Documents a Windows caution for polling; not executed here | Higher-level automation is unnecessary for byte-preserving transport |

These are versions observed in current primary documentation, not a claim that all alternatives were compiled. No alternate library was added to the workspace. The documentation lookup skill's ctx7 CLI could not authenticate with its configured API key; research used primary documentation and the downloaded portable-pty source instead.

## Verified behavior

The harmless fixture is `crates/uuldana-transport/src/bin/uuldana-test-child.rs`. The isolated probe accepts only `--scripted` or `--interactive` and launches that fixture, never an installed coding agent.

| Area | Local evidence | Boundary |
| --- | --- | --- |
| Native creation | ConPTY fixture starts and exits | Native `.exe`, inherited child environment overlay, absolute cwd |
| Input/output | `hello` reaches fixture and `ECHO=hello` returns | Raw output bytes; pipe mode has separate stderr, PTY merges streams |
| Full duplex | Child emits 2 MiB before consuming 2 MiB input; independent writer and reader both finish | Caller must drive output while using `InputHandle`; exclusive convenience writes are for small input |
| Resize | Child reports `SIZE=37x111` after resize | Positive dimensions through 32767; larger sizes rejected before backend conversion |
| ANSI | Escape bytes survive and green READY is present in the fixture stream | Visual rendering in a physical terminal has not been assessed |
| Keyboard | Example encoding test covers Unicode, Ctrl+C byte, arrow and release filtering | Physical key events, native signal delivery, mouse, function keys and extended keyboard protocols unverified |
| Pipe framing | CRLF/LF, final unterminated record, separate stderr, raw output | UTF-8 required only for Lines; 1 MiB maximum record; raw bytes need not be UTF-8 |
| Pressure | Concurrent 2 MiB stdout and 2 MiB stderr drain without loss | Bounded frame queue; consumer must continue receiving |
| Exit | Exit emitted once, last, after output drains; code 259 handled on Windows | Exit codes are process status, never usage-limit classifiers |
| Shutdown | Fixture quit is graceful; unresponsive mock and live PTY are force-terminated and reaped | Direct-child lifecycle is verified; descendant ownership is pending |
| Drop | Dropping a live pipe process removes the direct child | Emergency cleanup is asynchronous and produces no final history event |
| Environment / argv | Child overlay, removal, cwd and parent invariance; literal spaces, quotes, ampersand, percent and Unicode | Windows environment keys tested with ASCII and Unicode case variants |

## ConPTY and lifecycle details

[Microsoft's pseudoconsole guidance](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session) describes independent channel servicing, resize, final output during close, and cursor-inheritance query handling. Accordingly, reader and writer workers run independently; PTY close runs while output continues draining. The example/tests answer the initial `ESC[6n` query. A first experiment timed out before this reply was implemented. Arbitrary clients must relay terminal queries to their host terminal or answer them correctly. Dropping an unserviced cursor-inheriting PTY is not a verified cleanup path.

Inspection of portable-pty 0.9.0's `src/win/mod.rs` found two backend issues. Its nonblocking wait treats actual exit code 259 as STILL_ACTIVE; its cloned killer reverses the TerminateProcess return check. The Windows wrapper uses a dedicated handle-aware blocking waiter with cached status, and confirms termination through observed exit instead of trusting the killer's result. The original process handle remains owned until wait completes; the cloned killer targets that handle, not a potentially reused PID. Both exit-259 and forced PTY termination have regression tests. Unix retains serial child ownership rather than applying the Windows wait-thread workaround to PID-based killers.

On normal exit, the process closes new input, closes the PTY concurrently, drains output, and emits one final Exited frame. Shared input closure disables idle clones; writes already in progress can finish before EOF. Explicit supervisor stop records Stopping, tries adapter shutdown steps with deadlines, drains during grace, then schedules force termination and drains/reaps. Errors have typed transport/runtime variants. Reader failures are returned rather than silently represented as successful exits.

The prototype does not prove cleanup after supervisor OS termination, abort, power loss or hostile descendants. Windows Job Objects and Unix process groups, cancellation of blocked descendant-held pipes, and durable crash recovery need later lifecycle work. A decoder panic is caught and triggers process cleanup, but panic containment is not a security boundary for third-party Rust code.

## PTY bytes versus structured messages

A PTY carries a terminal screen protocol: merged streams, echo, cursor moves, partial writes, repainting and terminal queries. Chunk boundaries are not messages; ANSI-stripped output is not automatically a reliable transcript or a quota signal. The transport preserves bytes and does not manufacture native events. A future PTY adapter must label terminal heuristics Inferred and retain its own incremental decoder state.

Custom CLI Lines instead carries complete newline-delimited UTF-8 records plus separate raw stderr. The mock's documented tokens are structured Native events, while lifecycle and observed process exit are Synthetic. Its three limit scenarios and three provider-error scenarios all exit 1; distinct outcomes come from messages, demonstrating that exit codes cannot identify quota. No assumption is made that production agents supply JSON.

## Compatibility gaps and reproduction

ACP returns typed Unsupported. Native resume is deliberately unsupported by the mock. Windows `.cmd`/`.bat` launch under PTY is explicitly rejected because the portable-pty executable path does not provide a verified safe batch-shim launch. Pipe/probe launch uses standard-library argument handling; comprehensive batch escaping and installed-provider compatibility remain unverified. See [Rust Command documentation](https://doc.rust-lang.org/std/process/struct.Command.html#windows-argument-splitting) for the distinction between native argv and batch parsing. Its current web version is newer than the locally tested Rust 1.96.0.

From the workspace with a complete Rust toolchain:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace
cargo test --workspace --all-targets
cargo build -p uuldana-transport --bin uuldana-test-child
cargo run -p uuldana-transport --example pty_probe -- --scripted
# A real terminal is required; manual behavior is still unverified:
cargo run -p uuldana-transport --example pty_probe -- --interactive
```

The tested installation lacked GNU `as.exe`; the MSVC installation lacked the Windows SDK import libraries. Substituting LLVM dlltool allowed test binaries to run but crashed the standalone example before main with access violation 0xc0000005. Debugger evidence placed the crash in std's SetThreadDescription import loading. GNU dlltool with clang as an assembler resolved it. A reproducible, workspace-local optional workaround creates helpers only under ignored target and restores its environment:

```powershell
./scripts/verify-windows.ps1 -ClangPath 'D:/myproject/android-tools/Sdk/ndk/28.2.13676358/toolchains/llvm/prebuilt/windows-x86_64/bin/clang.exe'
```

On complete Windows toolchains, omit `-ClangPath`. The helper is toolchain repair for this host, not a runtime dependency or native provider setting. Logs are written to `target/verification/`. The CI matrix uses hosted standard toolchains and has not yet been executed remotely.
