# Contributing

Thanks for helping. This is a short guide to making a change that can be merged.

## Ground rules

- Uuldana supervises other agents. It does not become one. Changes that add an agent loop, a generic model wrapper or simulated provider responses will not be accepted.
- Do not claim more than you verified. A capability that was not proven against the real provider is `Unknown`, not `Supported`. Tests that need a provider that is not installed must skip and say so, never pass silently.
- `uuldana-core` stays synchronous, free of I/O and free of third-party dependencies (see ADR 0001 and 0005). The workspace forbids `unsafe_code`.

## Setup

You need a current stable Rust toolchain. On Windows follow [Windows development](docs/WINDOWS_DEVELOPMENT.md) for the GNU or MSVC prerequisites. Some tests use a helper process built by `scripts/prepare-test-helper.ps1`.

```
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

CI runs the same on Linux, macOS and Windows (GNU and MSVC).

## Tests

- Prefer real child processes in real pseudo terminals over mocks of the process layer. The runtime tests use small local programs that print what a provider's screen would print; the provider wording they react to comes from the real decoders.
- Tests that talk to an installed provider only open its interface and leave. They must not send a prompt, because that spends someone's quota.
- Tests must not leave data behind. Use temporary directories and remove them, and use realistic names and text, not placeholders that name a tool.

## Adding a provider

Start with the [Provider SDK](docs/PROVIDER_SDK.md). If the provider can be described by flags, a [manifest](docs/PROVIDER_SDK.md#provider-manifests) is enough. A compiled adapter needs:

1. flags verified against the provider's own `--help` or documentation, with the source named in the module comment;
2. an honest permission mapping (the mapping contract test in the existing adapters is the template);
3. detection that refuses script shims the PTY cannot host and recognizes the program by its version output;
4. capabilities limited to what you could show, and tests that run against the real program when it is installed.

## Commits and pull requests

Keep a change focused. Describe what changed and how you verified it, including what you could not verify. Do not reformat unrelated code.
