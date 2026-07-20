#!/usr/bin/env bash
# Integration tests: the canister inside PocketIC (docs/build-plan.md G2+).
# Builds the release wasm and the SOL RPC mock, finds the pocket-ic server
# that ships with dfx, and runs the #[ignore]-marked tests against it.
set -euo pipefail
cd "$(dirname "$0")/.."

# zstd-sys inside the SOL RPC client tree compiles C for wasm32; the shim
# picks system clang with a wasm backend or falls back to zig cc.
export CC_wasm32_unknown_unknown="$PWD/scripts/wasm-cc.sh"
export AR_wasm32_unknown_unknown="${AR_WASM32:-$(command -v llvm-ar || ls -d "$HOME"/.cache/solana/*/platform-tools/llvm/bin/llvm-ar 2>/dev/null | sort -V | tail -1 | grep . || echo "$HOME/.cache/zig/zig-ar")}"

# A second wasm baked with the fixture profile that leaves operator_wallet
# unset: the init override can pin an operator wallet but never unpin one,
# so the "operator methods are disabled" law needs its own build. Built
# first and copied aside, then the real wasm overwrites the shared path.
CROWN_PROFILE=../canister/tests/fixtures/no-operator \
    cargo build --target wasm32-unknown-unknown --release -p auction
cp target/wasm32-unknown-unknown/release/auction.wasm \
   target/wasm32-unknown-unknown/release/auction-no-operator.wasm

cargo build --target wasm32-unknown-unknown --release -p auction
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path canister/tests/mock-sol-rpc/Cargo.toml
cargo build --target wasm32-unknown-unknown --release \
    --manifest-path canister/tests/mock-index/Cargo.toml

if [ -z "${POCKET_IC_BIN:-}" ]; then
    POCKET_IC_BIN="$(ls -d "$HOME"/.cache/dfinity/versions/*/pocket-ic 2>/dev/null | sort -V | tail -1 || true)"
    export POCKET_IC_BIN
fi
[ -x "${POCKET_IC_BIN:-}" ] || {
    echo "pocket-ic binary not found; install dfx or set POCKET_IC_BIN" >&2
    exit 1
}

cargo test -p auction --test g2 --test g3 -- --include-ignored
