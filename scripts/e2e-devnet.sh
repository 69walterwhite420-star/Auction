#!/usr/bin/env bash
# G4 e2e (docs/build-plan.md): the auction against the real devnet.
#
# One local replica runs sol_rpc (Custom → devnet), crown-index (reading the
# real chain) and the game canister (the replica's threshold keys). Two
# auctions run on one clock — the main one (A) and the empty-winner one (Z).
# Acts:
#   1. a direct donate gives the donor the reputation they later vote with;
#   2. a phantom registration (no escrow born) is rejected by the real read;
#   3. lot A: donor's entry + donor2's top-up + a third entry the RECIPIENT
#      returns mid-bidding — its cancel signature claims(1) at once, and the
#      cancel signature does not open settle (a negative on the contract);
#   4. escrow X, born with a FOREIGN price of the game (fee_bps + 1): it
#      really exists on chain, and register_entry still refuses it — the
#      canister rebuilds the address from its own config and lands
#      elsewhere. Birth fields are the salt, so a foreign price is a
#      foreign escrow (§4, factory-spec §2.1);
#   5. escrow S, born with a DEADLINE shorter than the §9 floor: the
#      registration is refused for exactly that reason. S plays the two
#      halves of §15.12 later;
#   6. lot B (smaller) — both lots accepted; bidding ends; the finale picks
#      lot A; B resolves to cancel, its signature does not claim a foreign
#      escrow (a negative), claim(1) returns the money;
#   7. ready → the donor votes done → the tally settles; both A entries
#      claim(0) through the splitter; the RECIPIENT receives the sums net of the
#      game's fee;
#   8. §15.12 in full on S: the winner's standing verdict signs S too
#      (§15.10) though S was never registered. Before the DEADLINE the
#      signed settle cannot be undercut — refund() is refused; after it
#      refund() takes the gross back to the donor and the very same valid
#      signature is worthless. The book never hears about any of it;
#   9. auction Z — a winner with no live entries: lot C wins the finale with
#      two entries, then the OPERATOR returns both from VOTING. The auction
#      still tallies, and its verdict is settle over a zero live sum: the
#      lot's standing verdict is settle, while every real escrow of it
#      resolves to cancel and claims(1). Not a cent moves, the book is
#      silent;
#  10. the book credits EACH donor of the winning lot separately,
#      zero anomalies; crown-core and crown-factory work trees stay clean.
#
# Every book assertion is a DELTA from the baseline read at startup: the
# local replica is shared and never wiped, so what earlier runs credited
# these wallets is still there.
#
# THE LOCAL REPLICA IS SHARED AND IS NEVER WIPED HERE: threshold keys born
# by earlier runs may still resolve live devnet escrows. The script reuses a
# running replica or starts one over the existing state, and leaves it up.
#
# X is the one escrow whose money never comes back inside a run: reaching
# the derivation check at all demands a gross above the floor AND a deadline
# past the §9 floor, so its refund() door opens three days later. Hence
# X_GROSS = min_entry, the cheapest escrow that gets that far.
#
# With the local profile (voting_period = 120 s, duration = 420 s) the full
# run is ~18 min. Usage: scripts/e2e-devnet.sh
set -euo pipefail
cd "$(dirname "$0")/.."

SOL_RPC_URL=${SOL_RPC_URL:-https://api.devnet.solana.com}
SOL_DONOR_KEYPAIR=${SOL_DONOR_KEYPAIR:-$HOME/.cache/crown-e2e/donor.json}
# The RECIPIENT's permanent key (the same recipient as the sibling e2e): payouts and
# ATA rent stay recoverable between runs.
SOL_RECIPIENT_KEYPAIR=${SOL_RECIPIENT_KEYPAIR:-$HOME/.cache/crown-e2e/recipient.json}
# The permanent second donor; funded from the donor as needed.
SOL_DONOR2_KEYPAIR=${SOL_DONOR2_KEYPAIR:-$HOME/.cache/crown-e2e/donor2.json}
# The platform operator (game-spec §13). The testnet profile pins a wallet
# whose key nobody here holds, so the local profile pins this run's own key:
# the operator signs messages only and never touches the chain.
SOL_OPERATOR_KEYPAIR=${SOL_OPERATOR_KEYPAIR:-$HOME/.cache/crown-e2e/operator.json}
CORE=$(cd ../../Crown-Core && pwd)
FACTORY_REPO=$(cd ../../Crown-Factory && pwd)

VOTING_PERIOD=$(grep "^voting_period" config/testnet.toml | cut -d"=" -f2 | tr -d " ")
FEE_BPS=$(grep "^fee_bps" config/testnet.toml | cut -d"=" -f2 | tr -d " ")
FEE_WALLET=$(grep "^fee_wallet" config/testnet.toml | cut -d'"' -f2)
FACTORY=$(grep "^factory" config/testnet.toml | cut -d'"' -f2)
MARGIN=259200
# Amounts are sized to the devnet wallet; the donate alone clears
# MIN_VOTE_WEIGHT (100000) with a margin.
SOL_DONATE=200000
A1_GROSS=30000
A2_GROSS=20000
A3_GROSS=10000
B1_GROSS=5000
# The settle nobody executes (act 8): its gross comes back through refund().
S_GROSS=4000
# Auction Z's lot: both entries die by the operator's hand.
C1_GROSS=2000
C2_GROSS=1000
DURATION=420
PERFORM_WINDOW=180
MIN_ENTRY=1000
# The escrow with a foreign fee_bps. It has to clear every cheaper check the
# registration makes first — the floor and the deadline rule — to reach the
# derivation at all, so it is exactly at the floor and, by that same
# deadline rule, unrefundable inside a run.
X_GROSS=$MIN_ENTRY
TEXT_A=$(printf 'a1%.0s' $(seq 32))
TEXT_B=$(printf 'b2%.0s' $(seq 32))
TEXT_C=$(printf 'c3%.0s' $(seq 32))
NONCE=$(date +%s)

# ---- tooling ------------------------------------------------------------

participant() { cargo run -q -p auction --example participant -- "$@"; }
driver() { (cd e2e/solana-driver && cargo run -q -- "$@"); }

blob_hex() { # hex -> candid-блоб \xx
    python3 -c "import sys; h=sys.argv[1]; print(''.join(f'\\\\{h[i:i+2]}' for i in range(0,len(h),2)))" "$1"
}
b58_hex() { # base58 -> hex
    python3 - "$1" <<'EOF'
import sys
A = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
s = sys.argv[1]; n = 0
for c in s: n = n * 58 + A.index(c)
b = n.to_bytes(32, "big")
print(b.hex())
EOF
}
ok_blob() { # candid json (variant { Ok : blob }) со stdin -> hex или пусто
    python3 -c "
import json, sys
v = json.load(sys.stdin)
while isinstance(v, list) and len(v) == 1: v = v[0]
ok = v.get('Ok') if isinstance(v, dict) else None
if ok is None: print(); sys.exit()
if isinstance(ok, list) and all(isinstance(b, int) for b in ok):
    print(''.join(f'{b:02x}' for b in ok))
elif isinstance(ok, str):
    print(ok.removeprefix('0x'))
else:
    print()
"
}
err_text() { # candid json (variant { Err : text }) со stdin -> текст или пусто
    python3 -c "
import json, sys
v = json.load(sys.stdin)
while isinstance(v, list) and len(v) == 1: v = v[0]
print(v.get('Err', '') if isinstance(v, dict) else '')
"
}
verdict_field() { # field со stdin (json ответа request_signature) -> hex/имя
    python3 -c "
import json, sys
field = sys.argv[1]
v = json.load(sys.stdin)
while isinstance(v, list) and len(v) == 1: v = v[0]
ok = v.get('Ok') if isinstance(v, dict) else None
if not ok: print(); sys.exit()
value = ok.get(field)
if field == 'outcome':
    print(next(iter(value)) if isinstance(value, dict) and value else '')
elif isinstance(value, list) and value and all(isinstance(b, int) for b in value):
    print(''.join(f'{b:02x}' for b in value))
elif isinstance(value, str):
    print(value.removeprefix('0x'))
else:
    print()
" "$1"
}

game_call() { dfx canister call auction "$@"; }
resolver_of() { # auction_hex text_hash_hex -> resolver hex
    game_call get_resolver "(record {
        auction_id = blob \"$(blob_hex "$1")\";
        text_hash = blob \"$(blob_hex "$2")\" })" --output json | ok_blob
}
register_json() { # auction_hex text_hash_hex donor_hex gross deadline nonce
    game_call register_entry "(record {
        chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$1")\";
        text_hash = blob \"$(blob_hex "$2")\";
        donor = blob \"$(blob_hex "$3")\";
        gross = $4;
        deadline = $5;
        nonce = $6 })" --output json
}
register_entry() { # auction_hex text_hash_hex donor_hex gross deadline nonce -> escrow hex
    local json escrow err
    for _ in $(seq 1 18); do
        json=$(register_json "$@")
        escrow=$(echo "$json" | ok_blob)
        [ -n "$escrow" ] && { echo "$escrow"; return; }
        err=$(echo "$json" | err_text)
        # Finality lag: the driver confirms at "confirmed", the canister
        # reads "finalized" — retry until devnet finalizes the birth.
        echo "$err" | grep -q "does not exist" \
            || { echo "FAIL: registration rejected: $err" >&2; exit 1; }
        sleep 10
    done
    echo "FAIL: escrow never finalized" >&2
    exit 1
}
reputation() { # payer_blob recipient_blob
    dfx canister call crown-index get_reputation "(\"solana-devnet\", blob \"$1\", blob \"$2\")" \
        --query | tr -d '(_ )' | sed 's/:nat//'
}
request_sig_json() { # auction_hex text_hash_hex donor_hex gross deadline nonce
    game_call request_signature "(record {
        chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$1")\";
        text_hash = blob \"$(blob_hex "$2")\";
        donor = blob \"$(blob_hex "$3")\";
        gross = $4;
        deadline = $5;
        nonce = $6 })" --output json
}
await_signature() { # auction_hex text_hash_hex donor_hex gross deadline nonce expected -> sig hex
    local json outcome sig
    for _ in $(seq 1 30); do
        json=$(request_sig_json "$1" "$2" "$3" "$4" "$5" "$6" 2>/dev/null) || { sleep 10; continue; }
        outcome=$(echo "$json" | verdict_field outcome)
        sig=$(echo "$json" | verdict_field signature)
        if [ -n "$sig" ]; then
            [ "$outcome" = "$7" ] || { echo "FAIL: outcome $outcome, expected $7" >&2; exit 1; }
            echo "$sig"
            return
        fi
        sleep 10
    done
    echo "FAIL: signature never appeared" >&2
    exit 1
}
# The protocol message is UTF-8 text with newlines, so it travels by file: a
# shell argument would mangle it, and one stray byte is a different message.
wallet_sign() { # keypair auction_hex action [args...] -> sig hex
    local keypair=$1 auction=$2
    shift 2
    local msg sig
    msg=$(mktemp)
    participant auction-message solana-devnet "$GAME_ID" "$auction" "$@" > "$msg"
    sig=$(participant sol-sign "$keypair" "$msg")
    rm -f "$msg"
    echo "$sig"
}
recipient_sign() { wallet_sign "$SOL_RECIPIENT_KEYPAIR" "$@"; }
accept_lot() { # auction_hex lot_hex
    local sig
    sig=$(recipient_sign "$1" accept "$2")
    game_call accept_lot "(record { chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$1")\";
        lot_id = blob \"$(blob_hex "$2")\";
        signature = blob \"$(blob_hex "$sig")\" })" | grep -q Ok
}
# The finale is a timer: `ready` is refused until the scan has stood, so the
# retry doubles as the wait for it.
await_ready() { # auction_hex
    local sig out
    sig=$(recipient_sign "$1" ready)
    for _ in $(seq 1 20); do
        out=$(game_call ready "(record { chain = \"solana-devnet\";
            auction_id = blob \"$(blob_hex "$1")\";
            signature = blob \"$(blob_hex "$sig")\" })" 2>/dev/null || true)
        echo "$out" | grep -q "Ok" && return
        sleep 10
    done
    echo "FAIL: ready never landed for $1 (finale missing?)" >&2
    exit 1
}
donor_vote() { # auction_hex done|not_done
    local sig
    sig=$(wallet_sign "$SOL_DONOR_KEYPAIR" "$1" vote "$2")
    game_call vote "(record { chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$1")\";
        voter = blob \"$DONOR_BLOB\";
        choice = variant { $2 };
        signature = blob \"$(blob_hex "$sig")\" })" | grep -q Ok
}
operator_refund_entry() { # auction_hex escrow_hex
    local sig
    sig=$(wallet_sign "$SOL_OPERATOR_KEYPAIR" "$1" operator-refund-entry "$2")
    game_call operator_refund_entry "(record { chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$1")\";
        escrow = blob \"$(blob_hex "$2")\";
        signature = blob \"$(blob_hex "$sig")\" })" | grep -q Ok
}

# ---- keys and funding -----------------------------------------------------

DONOR=$(solana-keygen pubkey "$SOL_DONOR_KEYPAIR")
[ -f "$SOL_RECIPIENT_KEYPAIR" ] || solana-keygen new --no-bip39-passphrase --silent -o "$SOL_RECIPIENT_KEYPAIR"
RECIPIENT=$(solana-keygen pubkey "$SOL_RECIPIENT_KEYPAIR")
[ -f "$SOL_DONOR2_KEYPAIR" ] || solana-keygen new --no-bip39-passphrase --silent -o "$SOL_DONOR2_KEYPAIR"
DONOR2=$(solana-keygen pubkey "$SOL_DONOR2_KEYPAIR")
[ -f "$SOL_OPERATOR_KEYPAIR" ] || solana-keygen new --no-bip39-passphrase --silent -o "$SOL_OPERATOR_KEYPAIR"
OPERATOR=$(solana-keygen pubkey "$SOL_OPERATOR_KEYPAIR")
DONOR_HEX=$(b58_hex "$DONOR")
RECIPIENT_HEX=$(b58_hex "$RECIPIENT")
DONOR2_HEX=$(b58_hex "$DONOR2")
DONOR_BLOB=$(blob_hex "$DONOR_HEX")
RECIPIENT_BLOB=$(blob_hex "$RECIPIENT_HEX")
DONOR2_BLOB=$(blob_hex "$DONOR2_HEX")

echo "donor=$DONOR recipient=$RECIPIENT donor2=$DONOR2 operator=$OPERATOR"

echo "== fund donor2 if needed"
SOL_BAL=$(solana balance "$DONOR2" -u "$SOL_RPC_URL" | awk '{print $1}')
if python3 -c "import sys; sys.exit(0 if float('$SOL_BAL') < 0.02 else 1)"; then
    solana transfer -u "$SOL_RPC_URL" --keypair "$SOL_DONOR_KEYPAIR" \
        --allow-unfunded-recipient "$DONOR2" 0.03
fi
USDC=$(grep "^usdc" "$CORE/config/testnet.toml" | head -1 | cut -d'"' -f2)
USDC_BAL=$(driver balance "$SOL_RPC_URL" "$DONOR2")
if [ "$USDC_BAL" -lt "$A2_GROSS" ]; then
    spl-token transfer -u "$SOL_RPC_URL" --owner "$SOL_DONOR_KEYPAIR" \
        --fee-payer "$SOL_DONOR_KEYPAIR" --fund-recipient --allow-unfunded-recipient \
        "$USDC" 0.03 "$DONOR2"
fi

# ---- configs and builds ----------------------------------------------------

SPLITTER=$(grep "^splitter" "$CORE/config/testnet.toml" | head -1 | cut -d'"' -f2)

echo "== cursor seed and local profiles"
SOL_SEED=$(curl -s "$SOL_RPC_URL" -X POST -H "Content-Type: application/json" -d "{
    \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getSignaturesForAddress\",
    \"params\":[\"$SPLITTER\", {\"limit\": 1}]}" \
    | python3 -c "import json,sys; r=json.load(sys.stdin)['result']; print(r[0]['signature'] if r else '')")
# Empty when the splitter has no signatures the RPC still serves (fresh or
# pruned devnet): start ingest with no cursor and read the run's own donates.
if [ -n "$SOL_SEED" ]; then
    SEED_FIELD="cursor_seed = opt vec { record { \"solana-devnet\"; \"$SOL_SEED\" } };"
else
    SEED_FIELD=""
fi
echo "cursor seed: ${SOL_SEED:-<none, ingest from scratch>}"

cat > "$CORE/config/local.toml" <<EOF
# Generated by auction/scripts/e2e-devnet.sh; never committed.
[[chain]]
id        = "solana-devnet"
source    = "Custom:$SOL_RPC_URL"
consensus = "equality"
splitter  = "$SPLITTER"
usdc      = "$USDC"
$(grep '^factories' "$CORE/config/testnet.toml")
EOF

cat > config/local.toml <<EOF
# Generated by scripts/e2e-devnet.sh; never committed. The testnet profile
# with the read side pointed at devnet through the local sol_rpc canister
# and the operator wallet pinned to this run's own key.
threshold_key = "key_1"
voting_period = $VOTING_PERIOD
operator_wallet = "$OPERATOR"

[[chain]]
id      = "solana-devnet"
factory = "$FACTORY"
domain  = "crown:two-outcome:solana-devnet"
source    = "Custom:$SOL_RPC_URL"
consensus = "equality"
fee_bps    = $FEE_BPS
fee_wallet = "$FEE_WALLET"
EOF

# The wasm CC shim serves the canister builds; CROWN_PROFILE=local is scoped
# to them alone — the chain driver builds the factory crate, whose own
# profile must stay the default.
export CC_wasm32_unknown_unknown="$PWD/scripts/wasm-cc.sh"
export AR_wasm32_unknown_unknown="${AR_WASM32:-$(command -v llvm-ar || ls -d "$HOME"/.cache/solana/*/platform-tools/llvm/bin/llvm-ar 2>/dev/null | sort -V | tail -1 | grep . || echo "$HOME/.cache/zig/zig-ar")}"
echo "== build crown-index and the game (local profiles)"
(cd "$CORE" && CROWN_PROFILE=local \
    CC_wasm32_unknown_unknown="$CORE/scripts/wasm-cc.sh" \
    AR_wasm32_unknown_unknown="$AR_wasm32_unknown_unknown" \
    cargo build --target wasm32-unknown-unknown --release -p crown-index)

# ---- replica and canisters ------------------------------------------------

if ! dfx ping >/dev/null 2>&1; then
    echo "== starting the local replica over the EXISTING state (no --clean)"
    dfx start --background >/dev/null 2>&1
    for _ in $(seq 1 30); do dfx ping >/dev/null 2>&1 && break; sleep 1; done
fi
dfx ping >/dev/null 2>&1 || { echo "FAIL: replica did not come up" >&2; exit 1; }

dfx deploy sol_rpc
dfx deploy crown-index --argument "(opt record {
    sol_rpc = opt principal \"$(dfx canister id sol_rpc)\";
    $SEED_FIELD })"
dfx ledger fabricate-cycles --canister crown-index --t 100 >/dev/null
CROWN_PROFILE=local dfx deploy auction --argument "(opt record {
    sol_rpc = opt principal \"$(dfx canister id sol_rpc)\";
    crown_index = opt principal \"$(dfx canister id crown-index)\" })"
dfx ledger fabricate-cycles --canister auction --t 100 >/dev/null
GAME_ID=$(dfx canister id auction)
echo "game=$GAME_ID"

# ---- acts -----------------------------------------------------------------

# The replica is shared and never wiped, so the book accumulates across
# runs: every assertion below is relative to these baselines.
DONOR_BOOK0=$(reputation "$DONOR_BLOB" "$RECIPIENT_BLOB")
DONOR2_BOOK0=$(reputation "$DONOR2_BLOB" "$RECIPIENT_BLOB")
echo "book baselines: donor=$DONOR_BOOK0 donor2=$DONOR2_BOOK0"

echo "== direct donate: the reputation the donor will vote with"
driver donate "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$SOL_DONATE"

echo "== create both auctions on one clock"
AUCTION=$(participant auction-id "$GAME_ID" "$RECIPIENT_HEX" "$NONCE")
SIG=$(recipient_sign "$AUCTION" create "$NONCE" "$DURATION" "$PERFORM_WINDOW" "$MIN_ENTRY")
CREATED=$(date +%s)
game_call create_auction "(record {
    chain = \"solana-devnet\";
    recipient = blob \"$RECIPIENT_BLOB\";
    recipient_nonce = $NONCE;
    duration = $DURATION;
    perform_window = $PERFORM_WINDOW;
    min_entry = $MIN_ENTRY;
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok
DEADLINE=$((CREATED + DURATION + PERFORM_WINDOW + VOTING_PERIOD + MARGIN + 600))
Z_NONCE=$((NONCE + 100))
AUCTION_Z=$(participant auction-id "$GAME_ID" "$RECIPIENT_HEX" "$Z_NONCE")
SIG=$(recipient_sign "$AUCTION_Z" create "$Z_NONCE" "$DURATION" "$PERFORM_WINDOW" "$MIN_ENTRY")
game_call create_auction "(record {
    chain = \"solana-devnet\";
    recipient = blob \"$RECIPIENT_BLOB\";
    recipient_nonce = $Z_NONCE;
    duration = $DURATION;
    perform_window = $PERFORM_WINDOW;
    min_entry = $MIN_ENTRY;
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok
echo "auction=$AUCTION auction_z=$AUCTION_Z"

echo "== negative: a phantom registration is rejected by the real read"
PHANTOM_ERR=$(register_json "$AUCTION" "$TEXT_B" "$DONOR_HEX" "$B1_GROSS" "$DEADLINE" $((NONCE + 9)) | err_text)
echo "$PHANTOM_ERR" | grep -q "does not exist" || { echo "FAIL: phantom accepted: $PHANTOM_ERR"; exit 1; }

R_A=$(resolver_of "$AUCTION" "$TEXT_A")
R_B=$(resolver_of "$AUCTION" "$TEXT_B")
R_C=$(resolver_of "$AUCTION_Z" "$TEXT_C")
echo "resolver A=$R_A"
echo "resolver B=$R_B"
echo "resolver C=$R_C"
[ "$R_A" != "$R_B" ] || { echo "FAIL: resolvers must differ per lot"; exit 1; }
LOT_A=$(participant lot-id "$AUCTION" "$TEXT_A")
LOT_B=$(participant lot-id "$AUCTION" "$TEXT_B")
LOT_C=$(participant lot-id "$AUCTION_Z" "$TEXT_C")

echo "== lot A: the donor's entry, donor2's top-up, a third to return"
E_A1=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$A1_GROSS" "$DEADLINE" "$R_A" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 1)))
echo "escrow A1=$E_A1 (donor)"
REG=$(register_entry "$AUCTION" "$TEXT_A" "$DONOR_HEX" "$A1_GROSS" "$DEADLINE" $((NONCE + 1)))
[ "$REG" = "$(b58_hex "$E_A1")" ] || { echo "FAIL: A1 address parity"; exit 1; }
E_A2=$(driver create "$SOL_RPC_URL" "$SOL_DONOR2_KEYPAIR" "$RECIPIENT" "$A2_GROSS" "$DEADLINE" "$R_A" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 2)))
echo "escrow A2=$E_A2 (donor2)"
register_entry "$AUCTION" "$TEXT_A" "$DONOR2_HEX" "$A2_GROSS" "$DEADLINE" $((NONCE + 2)) >/dev/null
E_A3=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$A3_GROSS" "$DEADLINE" "$R_A" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 3)))
echo "escrow A3=$E_A3 (donor, to be returned)"
register_entry "$AUCTION" "$TEXT_A" "$DONOR_HEX" "$A3_GROSS" "$DEADLINE" $((NONCE + 3)) >/dev/null

echo "== negative: an escrow born with a FOREIGN price of the game is not this game's"
# Same donor, recipient, gross, deadline, resolver and nonce — one wrong
# fee_bps. The fee fields are part of the birth salt, so the canister, which
# rebuilds the address from its own config, derives a different address and
# finds nothing there. Its money waits out the DEADLINE by design.
E_X=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$X_GROSS" "$DEADLINE" "$R_A" $((FEE_BPS + 1)) "$FEE_WALLET" $((NONCE + 5)))
echo "escrow X=$E_X (fee_bps $((FEE_BPS + 1)), not $FEE_BPS)"
read -r X_CLOSED X_STATE_GROSS <<<"$(driver state "$SOL_RPC_URL" "$E_X")"
[ "$X_CLOSED" = "false" ] || { echo "FAIL: X born closed"; exit 1; }
[ "$X_STATE_GROSS" = "$X_GROSS" ] || { echo "FAIL: X gross $X_STATE_GROSS"; exit 1; }
X_ERR=$(register_json "$AUCTION" "$TEXT_A" "$DONOR_HEX" "$X_GROSS" "$DEADLINE" $((NONCE + 5)) | err_text)
echo "$X_ERR" | grep -q "does not exist" \
    || { echo "FAIL: a foreign-priced escrow was registered: $X_ERR"; exit 1; }

echo "== negative: the §9 deadline rule refuses a short-lived escrow"
# S is born past the finale and the vote but well inside the 72 h margin:
# registration is refused for exactly that reason, and S plays §15.12 later.
S_DEADLINE_AT=$((CREATED + DURATION + VOTING_PERIOD + 480))
E_S=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$S_GROSS" "$S_DEADLINE_AT" "$R_A" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 6)))
echo "escrow S=$E_S (DEADLINE at +$((S_DEADLINE_AT - CREATED))s)"
S_ERR=$(register_json "$AUCTION" "$TEXT_A" "$DONOR_HEX" "$S_GROSS" "$S_DEADLINE_AT" $((NONCE + 6)) | err_text)
echo "$S_ERR" | grep -q "escrow deadline too short" \
    || { echo "FAIL: a short deadline was registered: $S_ERR"; exit 1; }

echo "== lot B: the smaller rival"
E_B=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$B1_GROSS" "$DEADLINE" "$R_B" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 4)))
echo "escrow B=$E_B (donor)"
register_entry "$AUCTION" "$TEXT_B" "$DONOR_HEX" "$B1_GROSS" "$DEADLINE" $((NONCE + 4)) >/dev/null

echo "== auction Z, lot C: two entries the operator will kill"
E_C1=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$C1_GROSS" "$DEADLINE" "$R_C" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 7)))
register_entry "$AUCTION_Z" "$TEXT_C" "$DONOR_HEX" "$C1_GROSS" "$DEADLINE" $((NONCE + 7)) >/dev/null
E_C2=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$C2_GROSS" "$DEADLINE" "$R_C" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 8)))
register_entry "$AUCTION_Z" "$TEXT_C" "$DONOR_HEX" "$C2_GROSS" "$DEADLINE" $((NONCE + 8)) >/dev/null
echo "escrow C1=$E_C1 C2=$E_C2"

echo "== the RECIPIENT accepts every lot"
accept_lot "$AUCTION" "$LOT_A"
accept_lot "$AUCTION" "$LOT_B"
accept_lot "$AUCTION_Z" "$LOT_C"

echo "== the RECIPIENT returns entry A3 mid-bidding; its cancel claims at once"
SIG=$(recipient_sign "$AUCTION" return-entry "$(b58_hex "$E_A3")")
game_call return_entry "(record { chain = \"solana-devnet\";
    auction_id = blob \"$(blob_hex "$AUCTION")\";
    escrow = blob \"$(blob_hex "$(b58_hex "$E_A3")")\";
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok
SIG_A3=$(await_signature "$AUCTION" "$TEXT_A" "$DONOR_HEX" "$A3_GROSS" "$DEADLINE" $((NONCE + 3)) cancel)
if driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A3" 0 "$SIG_A3" "$R_A" >/dev/null 2>&1; then
    echo "FAIL: the cancel signature opened outcome 0"; exit 1
fi
BEFORE=$(driver balance "$SOL_RPC_URL" "$DONOR")
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A3" 1 "$SIG_A3" "$R_A"
[ "$(driver balance "$SOL_RPC_URL" "$DONOR")" = "$((BEFORE + A3_GROSS))" ] || { echo "FAIL: A3 refund"; exit 1; }

echo "== wait out the bidding window; the timer runs both finales"
NOW=$(date +%s)
LEFT=$((CREATED + DURATION - NOW))
[ "$LEFT" -gt 0 ] && { echo "sleeping ${LEFT}s + finale"; sleep $((LEFT + 5)); }
await_ready "$AUCTION"
VOTING_STARTED=$(date +%s)
await_ready "$AUCTION_Z"
echo "both finales stood: lot A and lot C perform"

echo "== the loser resolves the moment the finale stands"
SIG_B=$(await_signature "$AUCTION" "$TEXT_B" "$DONOR_HEX" "$B1_GROSS" "$DEADLINE" $((NONCE + 4)) cancel)
echo "== negative: B's signature does not claim a foreign escrow"
if driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A1" 1 "$SIG_B" "$R_B" >/dev/null 2>&1; then
    echo "FAIL: a foreign escrow accepted B's signature"; exit 1
fi
BEFORE=$(driver balance "$SOL_RPC_URL" "$DONOR")
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_B" 1 "$SIG_B" "$R_B"
[ "$(driver balance "$SOL_RPC_URL" "$DONOR")" = "$((BEFORE + B1_GROSS))" ] || { echo "FAIL: B refund"; exit 1; }

echo "== the OPERATOR empties the winning lot of auction Z, from VOTING"
# The operator's only direction is the donors' own money back — and it
# reaches the winner's entries even mid-vote (game-spec §13).
operator_refund_entry "$AUCTION_Z" "$(b58_hex "$E_C1")"
operator_refund_entry "$AUCTION_Z" "$(b58_hex "$E_C2")"

echo "== the donate ingest must land before the vote"
DONATED=$((DONOR_BOOK0 + SOL_DONATE))
REP=""
for _ in $(seq 1 90); do
    REP=$(reputation "$DONOR_BLOB" "$RECIPIENT_BLOB")
    echo "reputation: $REP/$DONATED"
    [ "$REP" = "$DONATED" ] && break
    sleep 10
done
[ "$REP" = "$DONATED" ] || { echo "FAIL: donate not ingested"; exit 1; }

echo "== the donor votes done in both auctions"
donor_vote "$AUCTION" done
donor_vote "$AUCTION_Z" done

echo "== wait out the voting period, then the settle signatures"
ELAPSED=$(($(date +%s) - VOTING_STARTED))
[ "$ELAPSED" -lt "$VOTING_PERIOD" ] && { echo "sleeping $((VOTING_PERIOD - ELAPSED + 60))s"; sleep $((VOTING_PERIOD - ELAPSED + 60)); }
SIG_A1=$(await_signature "$AUCTION" "$TEXT_A" "$DONOR_HEX" "$A1_GROSS" "$DEADLINE" $((NONCE + 1)) settle)
SIG_A2=$(await_signature "$AUCTION" "$TEXT_A" "$DONOR2_HEX" "$A2_GROSS" "$DEADLINE" $((NONCE + 2)) settle)

echo "== §15.12, the near side: refund() cannot undercut a standing settle"
# S was never registered, yet the winner's verdict signs it too (§15.10):
# the right to a signature is the derivation, not the registry.
# The near side is only a claim while the door is still shut: say so loudly
# rather than pass on a run that drifted past S's DEADLINE.
[ "$(date +%s)" -lt "$S_DEADLINE_AT" ] \
    || { echo "FAIL: the run drifted past S's DEADLINE before the near side"; exit 1; }
S_JSON=$(request_sig_json "$AUCTION" "$TEXT_A" "$DONOR_HEX" "$S_GROSS" "$S_DEADLINE_AT" $((NONCE + 6)))
SIG_S=$(echo "$S_JSON" | verdict_field signature)
[ "$(echo "$S_JSON" | verdict_field outcome)" = "settle" ] \
    || { echo "FAIL: the unregistered escrow of the winning lot did not settle"; exit 1; }
[ "$(echo "$S_JSON" | verdict_field escrow)" = "$(b58_hex "$E_S")" ] \
    || { echo "FAIL: the signed verdict names another escrow"; exit 1; }
read -r S_CLOSED S_STATE_GROSS <<<"$(driver state "$SOL_RPC_URL" "$E_S")"
[ "$S_CLOSED" = "false" ] || { echo "FAIL: S closed without a claim"; exit 1; }
[ "$S_STATE_GROSS" = "$S_GROSS" ] || { echo "FAIL: S gross $S_STATE_GROSS"; exit 1; }
if driver refund "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_S" >/dev/null 2>&1; then
    echo "FAIL: refund() beat the DEADLINE"; exit 1
fi

echo "== claim(0) on both A entries: the win moves through the splitter"
RECIPIENT_BEFORE=$(driver balance "$SOL_RPC_URL" "$RECIPIENT")
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A1" 0 "$SIG_A1" "$R_A"
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A2" 0 "$SIG_A2" "$R_A"
A1_PAYOUT=$((A1_GROSS - A1_GROSS * FEE_BPS / 10000))
A2_PAYOUT=$((A2_GROSS - A2_GROSS * FEE_BPS / 10000))
EXPECTED=$((RECIPIENT_BEFORE + A1_PAYOUT + A2_PAYOUT))
[ "$(driver balance "$SOL_RPC_URL" "$RECIPIENT")" = "$EXPECTED" ] || { echo "FAIL: recipient payout"; exit 1; }

echo "== auction Z: a winner with no live entries settles over a zero sum"
# The finale froze lot C as the winner while its entries were alive; the
# operator killed them afterwards. The tally still ran, and the lot's
# standing verdict is settle — for anything derivable to it that was not
# personally returned. Nothing is: both real escrows are cancel, and the
# settle applies only to escrows nobody ever created.
GHOST=$(await_signature "$AUCTION_Z" "$TEXT_C" "$DONOR_HEX" 1234 "$DEADLINE" $((NONCE + 10)) settle)
[ -n "$GHOST" ] || { echo "FAIL: the empty winner produced no verdict"; exit 1; }
RECIPIENT_AFTER_A=$(driver balance "$SOL_RPC_URL" "$RECIPIENT")
SIG_C1=$(await_signature "$AUCTION_Z" "$TEXT_C" "$DONOR_HEX" "$C1_GROSS" "$DEADLINE" $((NONCE + 7)) cancel)
SIG_C2=$(await_signature "$AUCTION_Z" "$TEXT_C" "$DONOR_HEX" "$C2_GROSS" "$DEADLINE" $((NONCE + 8)) cancel)
BEFORE=$(driver balance "$SOL_RPC_URL" "$DONOR")
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_C1" 1 "$SIG_C1" "$R_C"
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_C2" 1 "$SIG_C2" "$R_C"
[ "$(driver balance "$SOL_RPC_URL" "$DONOR")" = "$((BEFORE + C1_GROSS + C2_GROSS))" ] \
    || { echo "FAIL: the operator's returns did not come back whole"; exit 1; }
[ "$(driver balance "$SOL_RPC_URL" "$RECIPIENT")" = "$RECIPIENT_AFTER_A" ] \
    || { echo "FAIL: the empty winner moved money to the recipient"; exit 1; }

echo "== the book credits EACH donor of the winning lot"
# The book sees what reached the RECIPIENT: the direct donate whole, each
# settlement net of the game's fee; returns, refunds, the unexecuted S and
# the whole of auction Z never enter it.
DONOR_TOTAL=$((DONOR_BOOK0 + SOL_DONATE + A1_PAYOUT))
DONOR2_TOTAL=$((DONOR2_BOOK0 + A2_PAYOUT))
REP=""
for _ in $(seq 1 90); do
    REP=$(reputation "$DONOR_BLOB" "$RECIPIENT_BLOB")
    REP2=$(reputation "$DONOR2_BLOB" "$RECIPIENT_BLOB")
    echo "book: donor $REP/$DONOR_TOTAL donor2 $REP2/$DONOR2_TOTAL"
    [ "$REP" = "$DONOR_TOTAL" ] && [ "$REP2" = "$DONOR2_TOTAL" ] && break
    sleep 10
done
[ "$REP" = "$DONOR_TOTAL" ] || { echo "FAIL: donor attribution"; exit 1; }
[ "$REP2" = "$DONOR2_TOTAL" ] || { echo "FAIL: donor2 attribution"; exit 1; }

echo "== §15.12, the far side: after the DEADLINE refund() wins"
NOW=$(date +%s)
if [ "$NOW" -lt "$((S_DEADLINE_AT + 20))" ]; then
    echo "sleeping $((S_DEADLINE_AT + 20 - NOW))s for the DEADLINE"
    sleep $((S_DEADLINE_AT + 20 - NOW))
fi
DONOR_BEFORE=$(driver balance "$SOL_RPC_URL" "$DONOR")
# The chain's clock is its own; give the door a few tries to open.
for i in $(seq 1 5); do
    driver refund "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_S" && break
    [ "$i" = 5 ] && { echo "FAIL: refund never opened after the DEADLINE"; exit 1; }
    sleep 20
done
DONOR_AFTER=$(driver balance "$SOL_RPC_URL" "$DONOR")
[ "$DONOR_AFTER" = "$((DONOR_BEFORE + S_GROSS))" ] || { echo "FAIL: S's gross did not come back"; exit 1; }
read -r S_CLOSED _ <<<"$(driver state "$SOL_RPC_URL" "$E_S")"
[ "$S_CLOSED" = "true" ] || { echo "FAIL: S not terminal after refund"; exit 1; }
# The canister cannot revoke what it signed; the chain makes it worthless.
if driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_S" 0 "$SIG_S" "$R_A" >/dev/null 2>&1; then
    echo "FAIL: the settle signature still executed after the refund"; exit 1
fi
[ "$(driver balance "$SOL_RPC_URL" "$DONOR")" = "$DONOR_AFTER" ] \
    || { echo "FAIL: the stale settle signature moved money"; exit 1; }

echo "== the refunds and the empty winner left the book untouched; zero anomalies"
sleep 30
[ "$(reputation "$DONOR_BLOB" "$RECIPIENT_BLOB")" = "$DONOR_TOTAL" ] \
    || { echo "FAIL: the book moved after the refunds"; exit 1; }
[ "$(reputation "$DONOR2_BLOB" "$RECIPIENT_BLOB")" = "$DONOR2_TOTAL" ] \
    || { echo "FAIL: donor2's book moved after the refunds"; exit 1; }
ANOMALIES=$(dfx canister call crown-index get_anomaly_count --query | tr -d '(_ )' | sed 's/:nat64//')
[ "$ANOMALIES" = "0" ] || { echo "FAIL: anomaly count = $ANOMALIES"; exit 1; }

echo "== the trusted repositories stayed untouched"
git -C "$CORE" diff --quiet || { echo "FAIL: crown-core work tree dirty"; exit 1; }
git -C "$FACTORY_REPO" diff --quiet || { echo "FAIL: crown-factory work tree dirty"; exit 1; }

echo "e2e devnet OK"
