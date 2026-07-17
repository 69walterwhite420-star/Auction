#!/usr/bin/env bash
# Integration tests: the canister inside PocketIC (docs/build-plan.md G2+).
# Builds the release wasm and the SOL RPC mock, finds the pocket-ic server
# that ships with dfx, and runs the #[ignore]-marked tests against it.
set -euo pipefail
cd "$(dirname "$0")/.."

# zstd-sys inside the SOL RPC client tree compiles C for wasm32; the shim
# picks system clang with a wasm backend or falls back to zig cc.
export CC_wasm32_unknown_unknown="$PWD/scripts/wasm-cc.sh"
export AR_wasm32_unknown_unknown="${AR_WASM32:-$HOME/.cache/solana/v1.53/platform-tools/llvm/bin/llvm-ar}"

cargo build --target wasm32-unknown-unknown --release -p auction
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path canister/tests/mock-sol-rpc/Cargo.toml

if [ -z "${POCKET_IC_BIN:-}" ]; then
    POCKET_IC_BIN="$(ls -d "$HOME"/.cache/dfinity/versions/*/pocket-ic 2>/dev/null | sort -V | tail -1)"
    export POCKET_IC_BIN
fi
[ -x "${POCKET_IC_BIN:-}" ] || {
    echo "pocket-ic binary not found; install dfx or set POCKET_IC_BIN" >&2
    exit 1
}

cargo test -p auction --test g2 -- --include-ignored
