# 0007. Crate layout, naming and contract versioning

- Status: Accepted
- Date: 2026-10-12

## Context

The official product name is **Uuldana** (repository `uuldana-cli`, executable `uuldana`). The contracts live apart from the runtime so that third parties can write adapters against a stable surface, and out-of-tree adapters must be checked for compatibility explicitly.

## Decision

Workspace layout (`members = ["crates/*"]`, edition 2024, resolver 3):

| Crate | Purpose |
| --- | --- |
| `uuldana-core` | All contracts |
| `uuldana-provider-sdk` | Re-exports the adapter-facing subset of the core |
| `uuldana-cli` | `uuldana` binary |
| `uuldana-runtime` | Supervisor, process management, mock provider |
| `uuldana-transport` | PTY, ACP, custom CLI transports |
| `uuldana-providers` | Program discovery, learned profiles, manifest adapters |

Glob membership lets a new crate join without editing the root manifest.

Contract versioning:

- `uuldana_core::CONTRACT_VERSION` (`0.1` when this ADR was accepted; the current value is in `docs/CONTRACT_FREEZE.md`) versions the adapter contract independently of crate versions.
- Adapters declare the version they were built against in `ProviderDescriptor::contract`; `ProviderRegistry::register` rejects incompatible versions.
- While the major is `0`, any minor bump is breaking and minors must match exactly. From `1.0` on, minor bumps are additive and a host accepts adapters built for an older minor.
- Any change to a frozen public item bumps the contract version and is recorded in `docs/CONTRACT_FREEZE.md` with its reason and the dependent code that was checked.

Workspace lints: `unsafe_code = "forbid"`, `missing_docs = "warn"`, clippy `all = "warn"`. Validation runs clippy with `-D warnings`.

## Consequences

- Adapters depend on `uuldana-provider-sdk`; the core can move supervisor-side items without breaking them.
- A broken crate fails `cargo test --workspace` immediately, which is the desired integration signal.
- `missing_docs` plus `-D warnings` means every public item must be documented.
