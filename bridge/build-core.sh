#!/usr/bin/env bash
# Builds the Rust core, generates the UniFFI Swift bindings and drops the
# static library + Clang module where both the SwiftPM and Xcode builds
# expect them:
#   App/Sources/Generated/ToolboxCore.swift      Swift API over the core
#   App/Vendor/ToolboxCoreFFI/module.modulemap   C module the Swift file imports
#   App/Vendor/ToolboxCoreFFI/ToolboxCoreFFI.h
#   App/Vendor/libtoolbox_core.a                 static library to link
#
#   PROFILE=debug|release   (default release)
#   UNIVERSAL=1             also build x86_64 and lipo into a fat library
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$ROOT/core"
VENDOR="$ROOT/App/Vendor"
GEN="$ROOT/App/Sources/Generated"
PROFILE="${PROFILE:-release}"
export PATH="$HOME/.cargo/bin:$PATH"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-14.0}"
# Link with the Command Line Tools' clang; it needs no Xcode license.
unset DEVELOPER_DIR

HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
TARGETS=("$HOST_TRIPLE")
if [[ "${UNIVERSAL:-0}" == "1" ]]; then
  if [[ "$HOST_TRIPLE" == "aarch64-apple-darwin" ]]; then
    TARGETS+=(x86_64-apple-darwin)
  else
    TARGETS+=(aarch64-apple-darwin)
  fi
fi

PROFILE_FLAG=()
if [[ "$PROFILE" == "release" ]]; then
  PROFILE_FLAG=(--release)
fi

# The library that ships must not name this Mac: Rust records the source
# path of every crate for panic messages, and those sit under the home
# folder. Release builds rewrite them. Debug builds are left alone, since
# they share target/debug with `cargo test` and different flags would make
# the two rebuild each other's work. bridge/release.sh checks the result.
LIBRARY_RUSTFLAGS="${RUSTFLAGS:-}"
if [[ "$PROFILE" == "release" ]]; then
  CARGO_DIR="${CARGO_HOME:-$HOME/.cargo}"
  # rustc applies the last mapping that matches, so the general one goes first.
  LIBRARY_RUSTFLAGS+=" --remap-path-prefix=$HOME=/home --remap-path-prefix=$CARGO_DIR=/cargo --remap-path-prefix=$ROOT=/toolbox"
fi

cd "$CORE"
LIBS=()
for t in "${TARGETS[@]}"; do
  if [[ "$t" == "$HOST_TRIPLE" ]]; then
    RUSTFLAGS="$LIBRARY_RUSTFLAGS" cargo build -p toolbox-core "${PROFILE_FLAG[@]}"
    LIBS+=("$CORE/target/$PROFILE/libtoolbox_core.a")
    DYLIB="$CORE/target/$PROFILE/libtoolbox_core.dylib"
  else
    rustup target add "$t" >/dev/null 2>&1 || true
    RUSTFLAGS="$LIBRARY_RUSTFLAGS" cargo build -p toolbox-core "${PROFILE_FLAG[@]}" --target "$t"
    LIBS+=("$CORE/target/$t/$PROFILE/libtoolbox_core.a")
  fi
done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Bindings come from the cdylib's embedded metadata (UniFFI library mode).
cargo run -q -p toolbox-core --features cli --bin uniffi-bindgen -- \
  generate --library "$DYLIB" --language swift --out-dir "$TMP/gen"

mkdir -p "$VENDOR/ToolboxCoreFFI" "$GEN"
cp "$TMP/gen/ToolboxCore.swift" "$GEN/ToolboxCore.swift"
cp "$TMP/gen/ToolboxCoreFFI.h" "$VENDOR/ToolboxCoreFFI/ToolboxCoreFFI.h"
cp "$TMP/gen/ToolboxCoreFFI.modulemap" "$VENDOR/ToolboxCoreFFI/module.modulemap"

if [[ ${#LIBS[@]} -gt 1 ]]; then
  lipo -create "${LIBS[@]}" -output "$VENDOR/libtoolbox_core.a"
else
  cp "${LIBS[0]}" "$VENDOR/libtoolbox_core.a"
fi

echo "ok: $VENDOR/libtoolbox_core.a"
echo "ok: $GEN/ToolboxCore.swift"
