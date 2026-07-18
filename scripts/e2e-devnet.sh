#!/usr/bin/env bash
# G4 e2e (docs/build-plan.md): the auction against the real devnet.
#
# One local replica runs sol_rpc (Custom → devnet), crown-index (reading the
# real chain) and the game canister (the replica's threshold keys). Acts:
#   1. a direct donate gives the donor the reputation they later vote with;
#   2. a phantom registration (no escrow born) is rejected by the real read;
#   3. lot A: donor's entry + donor2's top-up + a third entry the RECIPIENT
#      returns mid-bidding — its cancel signature claims(1) at once, and the
#      cancel signature does not open settle (a negative on the contract);
#   4. lot B (smaller) — both lots accepted; bidding ends; the finale picks
#      lot A; B resolves to cancel, its signature does not claim a foreign
#      escrow (a negative), claim(1) returns the money;
#   5. ready → the donor votes done → the tally settles; both A entries
#      claim(0) through the splitter; the RECIPIENT receives the sums net of the
#      game's fee;
#   6. the book credits EACH donor of the winning lot separately,
#      zero anomalies; crown-core and crown-factory work trees stay clean.
#
# THE LOCAL REPLICA IS SHARED AND IS NEVER WIPED HERE: threshold keys born
# by earlier runs may still resolve live devnet escrows. The script reuses a
# running replica or starts one over the existing state, and leaves it up.
#
# With the local profile (voting_period = 120 s, duration = 420 s) the full
# run is ~15 min. Usage: scripts/e2e-devnet.sh
set -euo pipefail
cd "$(dirname "$0")/.."

SOL_RPC_URL=${SOL_RPC_URL:-https://api.devnet.solana.com}
SOL_DONOR_KEYPAIR=${SOL_DONOR_KEYPAIR:-$HOME/.cache/crown-e2e/donor.json}
# The RECIPIENT's permanent key (the same recipient as the sibling e2e): payouts and
# ATA rent stay recoverable between runs.
SOL_RECIPIENT_KEYPAIR=${SOL_RECIPIENT_KEYPAIR:-$HOME/.cache/crown-e2e/recipient.json}
# The permanent second donor; funded from the donor as needed.
SOL_DONOR2_KEYPAIR=${SOL_DONOR2_KEYPAIR:-$HOME/.cache/crown-e2e/donor2.json}
CORE=$(cd ../../Crown-Core && pwd)
FACTORY_REPO=$(cd ../../Crown-Factory && pwd)

VOTING_PERIOD=$(grep "^voting_period" config/testnet.toml | cut -d"=" -f2 | tr -d " ")
FEE_BPS=$(grep "^fee_bps" config/testnet.toml | cut -d"=" -f2 | tr -d " ")
FEE_WALLET=$(grep "^fee_wallet" config/testnet.toml | cut -d'"' -f2)
FACTORY=$(grep "^factory" config/testnet.toml | cut -d'"' -f2)
OPERATOR=$(grep "^operator_wallet" config/testnet.toml | cut -d'"' -f2)
MARGIN=259200
# Amounts are sized to the devnet wallet; the donate alone clears
# MIN_VOTE_WEIGHT (100000) with a margin.
SOL_DONATE=200000
A1_GROSS=30000
A2_GROSS=20000
A3_GROSS=10000
B1_GROSS=5000
DURATION=420
PERFORM_WINDOW=180
MIN_ENTRY=1000
TEXT_A=$(printf 'a1%.0s' $(seq 32))
TEXT_B=$(printf 'b2%.0s' $(seq 32))
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
resolver_of() { # text_hash_hex -> resolver hex
    game_call get_resolver "(record {
        auction_id = blob \"$(blob_hex "$AUCTION")\";
        text_hash = blob \"$(blob_hex "$1")\" })" --output json | ok_blob
}
register_json() { # text_hash_hex donor_hex gross deadline nonce
    game_call register_entry "(record {
        chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$AUCTION")\";
        text_hash = blob \"$(blob_hex "$1")\";
        donor = blob \"$(blob_hex "$2")\";
        gross = $3;
        deadline = $4;
        nonce = $5 })" --output json
}
register_entry() { # text_hash_hex donor_hex gross deadline nonce -> escrow hex
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
request_sig_json() { # text_hash_hex donor_hex gross deadline nonce
    game_call request_signature "(record {
        chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$AUCTION")\";
        text_hash = blob \"$(blob_hex "$1")\";
        donor = blob \"$(blob_hex "$2")\";
        gross = $3;
        deadline = $4;
        nonce = $5 })" --output json
}
await_signature() { # text_hash_hex donor_hex gross deadline nonce expected -> sig hex
    local json outcome sig
    for _ in $(seq 1 30); do
        json=$(request_sig_json "$1" "$2" "$3" "$4" "$5" 2>/dev/null) || { sleep 10; continue; }
        outcome=$(echo "$json" | verdict_field outcome)
        sig=$(echo "$json" | verdict_field signature)
        if [ -n "$sig" ]; then
            [ "$outcome" = "$6" ] || { echo "FAIL: outcome $outcome, expected $6" >&2; exit 1; }
            echo "$sig"
            return
        fi
        sleep 10
    done
    echo "FAIL: signature never appeared" >&2
    exit 1
}
recipient_sign() { # action [args...] -> sig hex; the message travels by file
    local msg sig
    msg=$(mktemp)
    participant auction-message solana-devnet "$GAME_ID" "$AUCTION" "$@" > "$msg"
    sig=$(participant sol-sign "$SOL_RECIPIENT_KEYPAIR" "$msg")
    rm -f "$msg"
    echo "$sig"
}

# ---- keys and funding -----------------------------------------------------

DONOR=$(solana-keygen pubkey "$SOL_DONOR_KEYPAIR")
[ -f "$SOL_RECIPIENT_KEYPAIR" ] || solana-keygen new --no-bip39-passphrase --silent -o "$SOL_RECIPIENT_KEYPAIR"
RECIPIENT=$(solana-keygen pubkey "$SOL_RECIPIENT_KEYPAIR")
[ -f "$SOL_DONOR2_KEYPAIR" ] || solana-keygen new --no-bip39-passphrase --silent -o "$SOL_DONOR2_KEYPAIR"
DONOR2=$(solana-keygen pubkey "$SOL_DONOR2_KEYPAIR")
DONOR_HEX=$(b58_hex "$DONOR")
RECIPIENT_HEX=$(b58_hex "$RECIPIENT")
DONOR2_HEX=$(b58_hex "$DONOR2")
DONOR_BLOB=$(blob_hex "$DONOR_HEX")
RECIPIENT_BLOB=$(blob_hex "$RECIPIENT_HEX")
DONOR2_BLOB=$(blob_hex "$DONOR2_HEX")

echo "donor=$DONOR recipient=$RECIPIENT donor2=$DONOR2"

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
    | python3 -c "import json,sys; print(json.load(sys.stdin)['result'][0]['signature'])")
echo "cursor seed: $SOL_SEED"

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
# with the read side pointed at devnet through the local sol_rpc canister.
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
export AR_wasm32_unknown_unknown="${AR_WASM32:-$HOME/.cache/solana/v1.53/platform-tools/llvm/bin/llvm-ar}"
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
    cursor_seed = opt vec { record { \"solana-devnet\"; \"$SOL_SEED\" } } })"
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

echo "== create the auction"
AUCTION=$(participant auction-id "$GAME_ID" "$RECIPIENT_HEX" "$NONCE")
SIG=$(recipient_sign create "$NONCE" "$DURATION" "$PERFORM_WINDOW" "$MIN_ENTRY")
CREATED=$(date +%s)
game_call create_auction "(record {
    chain = \"solana-devnet\";
    recipient = blob \"$RECIPIENT_BLOB\";
    recipient_nonce = $NONCE;
    duration = $DURATION;
    perform_window = $PERFORM_WINDOW;
    min_entry = $MIN_ENTRY;
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok
echo "auction=$AUCTION"
DEADLINE=$((CREATED + DURATION + PERFORM_WINDOW + VOTING_PERIOD + MARGIN + 600))

echo "== negative: a phantom registration is rejected by the real read"
PHANTOM_ERR=$(register_json "$TEXT_B" "$DONOR_HEX" "$B1_GROSS" "$DEADLINE" $((NONCE + 4)) | err_text)
echo "$PHANTOM_ERR" | grep -q "does not exist" || { echo "FAIL: phantom accepted: $PHANTOM_ERR"; exit 1; }

R_A=$(resolver_of "$TEXT_A")
R_B=$(resolver_of "$TEXT_B")
echo "resolver A=$R_A"
echo "resolver B=$R_B"
[ "$R_A" != "$R_B" ] || { echo "FAIL: resolvers must differ per lot"; exit 1; }
LOT_A=$(participant lot-id "$AUCTION" "$TEXT_A")
LOT_B=$(participant lot-id "$AUCTION" "$TEXT_B")

echo "== lot A: the donor's entry, donor2's top-up, a third to return"
E_A1=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$A1_GROSS" "$DEADLINE" "$R_A" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 1)))
echo "escrow A1=$E_A1 (donor)"
REG=$(register_entry "$TEXT_A" "$DONOR_HEX" "$A1_GROSS" "$DEADLINE" $((NONCE + 1)))
[ "$REG" = "$(b58_hex "$E_A1")" ] || { echo "FAIL: A1 address parity"; exit 1; }
E_A2=$(driver create "$SOL_RPC_URL" "$SOL_DONOR2_KEYPAIR" "$RECIPIENT" "$A2_GROSS" "$DEADLINE" "$R_A" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 2)))
echo "escrow A2=$E_A2 (donor2)"
register_entry "$TEXT_A" "$DONOR2_HEX" "$A2_GROSS" "$DEADLINE" $((NONCE + 2)) >/dev/null
E_A3=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$A3_GROSS" "$DEADLINE" "$R_A" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 3)))
echo "escrow A3=$E_A3 (donor, to be returned)"
register_entry "$TEXT_A" "$DONOR_HEX" "$A3_GROSS" "$DEADLINE" $((NONCE + 3)) >/dev/null

echo "== lot B: the smaller rival"
E_B=$(driver create "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$RECIPIENT" "$B1_GROSS" "$DEADLINE" "$R_B" "$FEE_BPS" "$FEE_WALLET" $((NONCE + 4)))
echo "escrow B=$E_B (donor)"
register_entry "$TEXT_B" "$DONOR_HEX" "$B1_GROSS" "$DEADLINE" $((NONCE + 4)) >/dev/null

echo "== the RECIPIENT accepts both lots"
SIG=$(recipient_sign accept "$LOT_A")
game_call accept_lot "(record { chain = \"solana-devnet\";
    auction_id = blob \"$(blob_hex "$AUCTION")\";
    lot_id = blob \"$(blob_hex "$LOT_A")\";
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok
SIG=$(recipient_sign accept "$LOT_B")
game_call accept_lot "(record { chain = \"solana-devnet\";
    auction_id = blob \"$(blob_hex "$AUCTION")\";
    lot_id = blob \"$(blob_hex "$LOT_B")\";
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok

echo "== the RECIPIENT returns entry A3 mid-bidding; its cancel claims at once"
SIG=$(recipient_sign return-entry "$(b58_hex "$E_A3")")
game_call return_entry "(record { chain = \"solana-devnet\";
    auction_id = blob \"$(blob_hex "$AUCTION")\";
    escrow = blob \"$(blob_hex "$(b58_hex "$E_A3")")\";
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok
SIG_A3=$(await_signature "$TEXT_A" "$DONOR_HEX" "$A3_GROSS" "$DEADLINE" $((NONCE + 3)) cancel)
if driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A3" 0 "$SIG_A3" "$R_A" >/dev/null 2>&1; then
    echo "FAIL: the cancel signature opened outcome 0"; exit 1
fi
BEFORE=$(driver balance "$SOL_RPC_URL" "$DONOR")
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A3" 1 "$SIG_A3" "$R_A"
[ "$(driver balance "$SOL_RPC_URL" "$DONOR")" = "$((BEFORE + A3_GROSS))" ] || { echo "FAIL: A3 refund"; exit 1; }

echo "== wait out the bidding window; the timer runs the finale"
NOW=$(date +%s)
LEFT=$((CREATED + DURATION - NOW))
[ "$LEFT" -gt 0 ] && { echo "sleeping ${LEFT}s + finale"; sleep $((LEFT + 5)); }
READY_SIG=$(recipient_sign ready)
for _ in $(seq 1 20); do
    OUT=$(game_call ready "(record { chain = \"solana-devnet\";
        auction_id = blob \"$(blob_hex "$AUCTION")\";
        signature = blob \"$(blob_hex "$READY_SIG")\" })" 2>/dev/null || true)
    echo "$OUT" | grep -q "Ok" && break
    sleep 10
done
echo "$OUT" | grep -q "Ok" || { echo "FAIL: ready never landed (finale missing?)"; exit 1; }
VOTING_STARTED=$(date +%s)
echo "the finale stood: lot A performs"

echo "== the loser resolves the moment the finale stands"
SIG_B=$(await_signature "$TEXT_B" "$DONOR_HEX" "$B1_GROSS" "$DEADLINE" $((NONCE + 4)) cancel)
echo "== negative: B's signature does not claim a foreign escrow"
if driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A1" 1 "$SIG_B" "$R_B" >/dev/null 2>&1; then
    echo "FAIL: a foreign escrow accepted B's signature"; exit 1
fi
BEFORE=$(driver balance "$SOL_RPC_URL" "$DONOR")
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_B" 1 "$SIG_B" "$R_B"
[ "$(driver balance "$SOL_RPC_URL" "$DONOR")" = "$((BEFORE + B1_GROSS))" ] || { echo "FAIL: B refund"; exit 1; }

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

echo "== the donor votes done"
MSG=$(mktemp)
participant auction-message solana-devnet "$GAME_ID" "$AUCTION" vote done > "$MSG"
SIG=$(participant sol-sign "$SOL_DONOR_KEYPAIR" "$MSG")
rm -f "$MSG"
game_call vote "(record { chain = \"solana-devnet\";
    auction_id = blob \"$(blob_hex "$AUCTION")\";
    voter = blob \"$DONOR_BLOB\";
    choice = variant { done };
    signature = blob \"$(blob_hex "$SIG")\" })" | grep -q Ok

echo "== wait out the voting period, then the settle signatures"
ELAPSED=$(($(date +%s) - VOTING_STARTED))
[ "$ELAPSED" -lt "$VOTING_PERIOD" ] && { echo "sleeping $((VOTING_PERIOD - ELAPSED + 60))s"; sleep $((VOTING_PERIOD - ELAPSED + 60)); }
SIG_A1=$(await_signature "$TEXT_A" "$DONOR_HEX" "$A1_GROSS" "$DEADLINE" $((NONCE + 1)) settle)
SIG_A2=$(await_signature "$TEXT_A" "$DONOR2_HEX" "$A2_GROSS" "$DEADLINE" $((NONCE + 2)) settle)

echo "== claim(0) on both A entries: the win moves through the splitter"
KM_BEFORE=$(driver balance "$SOL_RPC_URL" "$RECIPIENT")
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A1" 0 "$SIG_A1" "$R_A"
driver claim "$SOL_RPC_URL" "$SOL_DONOR_KEYPAIR" "$E_A2" 0 "$SIG_A2" "$R_A"
A1_PAYOUT=$((A1_GROSS - A1_GROSS * FEE_BPS / 10000))
A2_PAYOUT=$((A2_GROSS - A2_GROSS * FEE_BPS / 10000))
EXPECTED=$((KM_BEFORE + A1_PAYOUT + A2_PAYOUT))
[ "$(driver balance "$SOL_RPC_URL" "$RECIPIENT")" = "$EXPECTED" ] || { echo "FAIL: recipient payout"; exit 1; }

echo "== the book credits EACH donor of the winning lot"
# The book sees what reached the RECIPIENT: the direct donate whole, each
# settlement net of the game's fee; returns and refunds never enter it.
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

ANOMALIES=$(dfx canister call crown-index get_anomaly_count --query | tr -d '(_ )' | sed 's/:nat64//')
[ "$ANOMALIES" = "0" ] || { echo "FAIL: anomaly count = $ANOMALIES"; exit 1; }

echo "== the trusted repositories stayed untouched"
git -C "$CORE" diff --quiet || { echo "FAIL: crown-core work tree dirty"; exit 1; }
git -C "$FACTORY_REPO" diff --quiet || { echo "FAIL: crown-factory work tree dirty"; exit 1; }

echo "e2e devnet OK"
