#!/usr/bin/env sh
# Builds the release binary and packs it with its documentation into a tar.gz
# and a SHA256 checksum file under target/dist.
#
# Builds locally only. It publishes nothing and contacts no service.
#
# Usage: scripts/package-release.sh [target-triple]
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# An explicit target switches Cargo to cross-compilation mode. Without one the
# build is native, exactly as `cargo build` does.
explicit=${1:-}
target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || { echo "could not read the workspace version from Cargo.toml" >&2; exit 1; }

# Source paths inside the binary are relative, so a build does not carry the
# home directory of whoever made it.
cargo_home=${CARGO_HOME:-$HOME/.cargo}
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$root=uuldana --remap-path-prefix=$cargo_home=cargo --remap-path-prefix=$(rustc --print sysroot)=rustc"

if [ -n "$explicit" ]; then
    cargo build --release --locked -p uuldana-cli --target "$target"
    binary="target/$target/release/uuldana"
else
    cargo build --release --locked -p uuldana-cli
    binary="target/release/uuldana"
fi
[ -f "$binary" ] || { echo "missing build output: $binary" >&2; exit 1; }
if grep -a -q -e "$HOME" -e "$(id -un)" "$binary"; then
    echo "$binary contains the home directory or user name of this machine" >&2
    exit 1
fi

name="uuldana-$version-$target"
dist="target/dist"
stage="$dist/$name"
rm -rf "$stage"
mkdir -p "$stage/docs"

cp "$binary" "$stage/"
cp README.md LICENSE-MIT THIRD_PARTY_NOTICES.md SECURITY.md "$stage/"
cp docs/CONFIGURATION.md docs/PLUGIN_COMPATIBILITY.md docs/PROVIDER_SDK.md "$stage/docs/"
cp -R examples "$stage/"

archive="$dist/$name.tar.gz"
tar -C "$dist" -czf "$archive" "$name"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$dist" && sha256sum "$name.tar.gz" > "$name.sha256")
else
    (cd "$dist" && shasum -a 256 "$name.tar.gz" > "$name.sha256")
fi

echo "archive : $archive"
echo "sha256  : $(cut -d' ' -f1 "$dist/$name.sha256")"
