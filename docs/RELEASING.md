# Releasing

This describes how a release is prepared. Nothing here publishes anything by itself: building archives is automatic, publishing is a person's decision.

## Before a release

1. All checks pass on every platform in the CI matrix:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --locked -- -D warnings`
   - `cargo test --workspace --locked`
2. The version in the workspace `Cargo.toml` is the version being released, and `Cargo.lock` is committed and current.
3. [README](../README.md) provider table and platform notes match what was actually verified. A provider that was not run against the real program stays marked as such.
4. `cargo audit` reports nothing for `Cargo.lock`, and `scripts/third-party-notices.ps1` was run after the last dependency change so `THIRD_PARTY_NOTICES.md` names every library built into the archives (it covers Windows, Linux and macOS).
5. The release binary holds no path of the machine that built it. Build it through `scripts/release-env.ps1` (or `scripts/package-release.*`), which remaps the paths and checks the result.
6. Run the manual checks that CI cannot do: open each installed provider through `uuldana`, quit it, and run a session with `uuldana resume`. A real failover needs a real exhausted quota; if it was not exercised, the release notes say so.

## Building archives

On a machine with a Rust toolchain:

```
# Windows (the GNU toolchain also needs the MinGW tools on PATH, see WINDOWS_DEVELOPMENT.md)
./scripts/package-release.ps1

# Linux and macOS
sh scripts/package-release.sh
```

Each script builds `uuldana-cli` in release mode with `--locked`, and writes to `target/dist`:

- `uuldana-<version>-<target>.zip` (Windows) or `.tar.gz` (Unix), holding the binary, README, the license and the third party notices, the security policy, the configuration, plugin and SDK documents, and the examples;
- `uuldana-<version>-<target>.sha256`, the archive's SHA-256.

Pass a target triple as the first argument to cross-compile. Without one the build is native.

The `Release archives` workflow runs the same scripts on Linux, macOS and Windows for a manual run or a `v*` tag, smoke-tests the packaged binary with `--version`, and keeps the archives as workflow artifacts. It never creates a release or uploads elsewhere.

## Publishing to npm

The Windows x64 package `uuldana-cli` is published by the `Publish to npm` workflow (`.github/workflows/release-npm.yml`). Every push to `main` runs a short first job that reads the version in `Cargo.toml` and asks npm whether that version is already published. If it is, nothing else happens. If it is not, a second job runs format, clippy and the tests, builds the binary with the path remapping of `scripts/release-env.ps1`, packs it with `packaging/npm/build.ps1`, installs the pack in a clean folder and checks `uuldana --version`, and then runs `npm publish`. Before it publishes, the job also asks npm whether it trusts the workflow and prints npm's own reason if it does not.

Login is npm Trusted Publishing, so no token is stored in the repository. Set it up once on npmjs.com: package `uuldana-cli`, Settings, Trusted Publisher, GitHub Actions, owner `Adjie-Dev`, repository `uuldana-cli`, workflow `release-npm.yml`. Without it the publish step is refused.

To release:

1. Raise `version` in `Cargo.toml` (and the two `version` entries under `[workspace.dependencies]`), run `cargo check --workspace` so `Cargo.lock` follows, and commit.
2. Push to `main`. The workflow publishes the new version by itself.

npm refuses a version that was published before, so a push that does not raise the version publishes nothing, and the workflow says so in its summary. Every change that should reach npm, including a README change, needs a new version. A manual run (Actions, `Publish to npm`, Run workflow) is a dry run by default: it builds and packs everything and publishes nothing.

## Other publishing

Before the repository is made public it needs a contact in [SECURITY.md](../SECURITY.md) that is not one person's mailbox if the project grows past one maintainer. Release binaries are not code signed; say so in the notes until they are.

A GitHub release or a download page is not automated. The crates carry `publish = false`; remove that only for the crates that are meant to be published. Attach the archives and their `.sha256` files, and state in the notes which platforms and providers were verified and which were not.
