//! Shared harness of the PocketIC integration tests: instance setup with the
//! SOL RPC mock, wallet signing (an independent re-implementation of the
//! wallet side), escrow fixtures planted into the mock, auction flows and
//! full offchain certificate verification.

#![allow(dead_code)] // each test binary uses its own subset

use auction::api::{
    AuctionActionArg, CertifiedRecord, CreateAuctionArg, EntryActionArg, GetResolverArg,
    LotActionArg, RegisterEntryArg,
};
use auction::{AuctionRecord, EntryRecord, LotRecord, auction_key, auth};
use candid::{Decode, Encode, Principal};
use ic_certification::{Certificate, HashTree, LookupResult};
use pocket_ic::{PocketIc, PocketIcBuilder};
use serde_bytes::ByteBuf;
use sha2::{Digest, Sha256};

pub const CHAIN: &str = "solana-devnet";
pub const DURATION: u64 = 86_400;
pub const PERFORM_WINDOW: u64 = 43_200;
pub const MIN_BID: u64 = 50;
/// Mirrors config/testnet.toml — the profile the test wasm is baked with.
pub const VOTING_PERIOD: u64 = 120;
pub const FACTORY: &str = "83f7ziVs5VeQ8xiDka8zczbfJT4WcxsXQ18cqWwmV5ur";
pub const FEE_BPS: u16 = 300;
pub const FEE_WALLET: &str = "3it64t7KXNip1C1BRYNh8ygeKyujWnaQrPSj3hV9TWbE";
/// Mirrors logic::DEADLINE_MARGIN.
pub const DEADLINE_MARGIN: u64 = 259_200;

/// Escrow account layout of the two-outcome shape (factory lib.rs):
/// discriminator ‖ donor ‖ salt ‖ streamer ‖ resolver ‖ gross ‖ deadline ‖
/// fee_bps ‖ fee_wallet ‖ bump ‖ settled.
pub const ESCROW_DISCRIMINATOR: [u8; 8] = [31, 213, 123, 187, 186, 22, 218, 155];

// ---- instances ----------------------------------------------------------------

pub fn game_wasm() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../target/wasm32-unknown-unknown/release/auction.wasm"
    );
    std::fs::read(path).expect("wasm missing: run scripts/test-canister.sh")
}

pub fn mock_wasm() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/mock-sol-rpc/target/wasm32-unknown-unknown/release/mock_sol_rpc.wasm"
    );
    std::fs::read(path).expect("mock wasm missing: run scripts/test-canister.sh")
}

pub fn mock_index_wasm() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/mock-index/target/wasm32-unknown-unknown/release/mock_crown_index.wasm"
    );
    std::fs::read(path).expect("mock index wasm missing: run scripts/test-canister.sh")
}

/// The operator wallet every test instance is installed with, via the init
/// override — the baked testnet key's secret lives outside the repo.
pub fn operator() -> Wallet {
    wallet(0xE0)
}

pub struct Setup {
    pub pic: PocketIc,
    pub game: Principal,
    pub rpc: Principal,
    pub index: Principal,
}

/// A game canister wired to the SOL RPC and crown-index mocks via the init
/// overrides. Everything sits on the NNS subnet so certificates carry no
/// delegation; the II subnet provides the threshold keys.
pub fn setup() -> Setup {
    let pic = PocketIcBuilder::new()
        .with_nns_subnet()
        .with_ii_subnet()
        .build();
    let nns = pic.topology().get_nns().expect("nns subnet");

    let rpc = pic.create_canister_on_subnet(None, None, nns);
    pic.add_cycles(rpc, 10_000_000_000_000);
    pic.install_canister(rpc, mock_wasm(), Encode!().unwrap(), None);

    let index = pic.create_canister_on_subnet(None, None, nns);
    pic.add_cycles(index, 10_000_000_000_000);
    pic.install_canister(index, mock_index_wasm(), Encode!().unwrap(), None);

    let game = pic.create_canister_on_subnet(None, None, nns);
    pic.add_cycles(game, 10_000_000_000_000);
    let overrides = auction::Overrides {
        sol_rpc: Some(rpc),
        crown_index: Some(index),
        operator_wallet: Some(ByteBuf::from(operator().address)),
    };
    pic.install_canister(game, game_wasm(), Encode!(&Some(overrides)).unwrap(), None);
    Setup {
        pic,
        game,
        rpc,
        index,
    }
}

pub fn seed_reputation(s: &Setup, voter: &[u8], km: &[u8], value: u128) {
    let arg = Encode!(
        &CHAIN.to_string(),
        &ByteBuf::from(voter.to_vec()),
        &ByteBuf::from(km.to_vec()),
        &value
    )
    .unwrap();
    s.pic
        .update_call(s.index, Principal::anonymous(), "set_reputation", arg)
        .expect("seed reputation");
}

pub fn now_seconds(pic: &PocketIc) -> u64 {
    pic.get_time().as_nanos_since_unix_epoch() / 1_000_000_000
}

pub fn update<R: for<'a> candid::utils::ArgumentDecoder<'a>>(
    pic: &PocketIc,
    canister: Principal,
    method: &str,
    arg: Vec<u8>,
) -> R {
    let reply = pic
        .update_call(canister, Principal::anonymous(), method, arg)
        .unwrap_or_else(|reject| panic!("{method} rejected: {reject:?}"));
    candid::utils::decode_args(&reply).expect("reply decodes")
}

pub fn query<R: for<'a> candid::utils::ArgumentDecoder<'a>>(
    pic: &PocketIc,
    canister: Principal,
    method: &str,
    arg: Vec<u8>,
) -> R {
    let reply = pic
        .query_call(canister, Principal::anonymous(), method, arg)
        .unwrap_or_else(|reject| panic!("{method} rejected: {reject:?}"));
    candid::utils::decode_args(&reply).expect("reply decodes")
}

// ---- wallets ------------------------------------------------------------------

pub struct Wallet {
    pub key: ed25519_dalek::SigningKey,
    pub address: Vec<u8>,
}

pub fn wallet(seed: u8) -> Wallet {
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let address = key.verifying_key().to_bytes().to_vec();
    Wallet { key, address }
}

/// Raw Ed25519 over the protocol message, re-implemented independently of
/// auth.rs.
pub fn sign(wallet: &Wallet, message: &[u8]) -> Vec<u8> {
    use ed25519_dalek::Signer;
    wallet.key.sign(message).to_bytes().to_vec()
}

// ---- flows -------------------------------------------------------------------

pub fn create_auction(s: &Setup, km: &Wallet, km_nonce: u64) -> Result<Vec<u8>, String> {
    let auction_id = auth::derive_auction_id(s.game.as_slice(), &km.address, km_nonce).unwrap();
    let message = auth::auction_message(
        CHAIN,
        &s.game.to_text(),
        &auction_id,
        &auth::Action::Create {
            km_nonce,
            duration: DURATION,
            perform_window: PERFORM_WINDOW,
            min_bid: MIN_BID,
        },
    );
    let arg = CreateAuctionArg {
        chain: CHAIN.to_string(),
        km: ByteBuf::from(km.address.clone()),
        km_nonce,
        duration: DURATION,
        perform_window: PERFORM_WINDOW,
        min_bid: MIN_BID,
        signature: ByteBuf::from(sign(km, message.as_bytes())),
    };
    let (result,): (Result<ByteBuf, String>,) =
        update(&s.pic, s.game, "create_auction", Encode!(&arg).unwrap());
    result.map(|id| {
        assert_eq!(id.as_slice(), auction_id.as_slice(), "id parity");
        auction_id.to_vec()
    })
}

pub fn get_resolver(s: &Setup, auction_id: &[u8], text_hash: &[u8]) -> Result<Vec<u8>, String> {
    let arg = GetResolverArg {
        auction_id: ByteBuf::from(auction_id.to_vec()),
        text_hash: ByteBuf::from(text_hash.to_vec()),
    };
    let (result,): (Result<ByteBuf, String>,) =
        update(&s.pic, s.game, "get_resolver", Encode!(&arg).unwrap());
    result.map(|key| key.to_vec())
}

/// The escrow deadline floor of an auction created at `created_at`
/// (game-spec §9).
pub fn good_deadline(created_at: u64) -> u64 {
    created_at + DURATION + PERFORM_WINDOW + VOTING_PERIOD + DEADLINE_MARGIN
}

/// The salt of the two-outcome shape, re-implemented independently of
/// crown-salt: sha256(donor ‖ streamer ‖ gross_le ‖ deadline_le ‖ resolver ‖
/// fee_bps_le ‖ fee_wallet ‖ nonce_le).
pub fn derive_salt(
    donor: &[u8],
    km: &[u8],
    gross: u64,
    deadline: u64,
    resolver: &[u8],
    nonce: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(donor);
    hasher.update(km);
    hasher.update(gross.to_le_bytes());
    hasher.update((deadline as i64).to_le_bytes());
    hasher.update(resolver);
    hasher.update(FEE_BPS.to_le_bytes());
    hasher.update(bs58::decode(FEE_WALLET).into_vec().unwrap());
    hasher.update(nonce.to_le_bytes());
    hasher.finalize().into()
}

pub fn derive_escrow_address(salt: &[u8; 32]) -> Vec<u8> {
    let program: [u8; 32] = bs58::decode(FACTORY)
        .into_vec()
        .unwrap()
        .try_into()
        .unwrap();
    let (address, _) = crown_derive::solana_pda_address(program, &[b"escrow", salt]).unwrap();
    address.to_vec()
}

/// The raw account data of a two-outcome escrow with the given birth.
pub fn escrow_account_data(
    donor: &[u8],
    salt: &[u8; 32],
    km: &[u8],
    resolver: &[u8],
    gross: u64,
    deadline: u64,
    settled: bool,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(188);
    data.extend_from_slice(&ESCROW_DISCRIMINATOR);
    data.extend_from_slice(donor);
    data.extend_from_slice(salt);
    data.extend_from_slice(km);
    data.extend_from_slice(resolver);
    data.extend_from_slice(&gross.to_le_bytes());
    data.extend_from_slice(&(deadline as i64).to_le_bytes());
    data.extend_from_slice(&FEE_BPS.to_le_bytes());
    data.extend_from_slice(&bs58::decode(FEE_WALLET).into_vec().unwrap());
    data.push(255); // bump
    data.push(u8::from(settled));
    data
}

/// Plants an account into the SOL RPC mock at the given address, with the
/// given owner and data.
pub fn plant_account(s: &Setup, address: &[u8], owner: &str, data: &[u8]) {
    use base64::Engine;
    let account = sol_rpc_types::AccountInfo {
        lamports: 4_000_000,
        data: sol_rpc_types::AccountData::Binary(
            base64::engine::general_purpose::STANDARD.encode(data),
            sol_rpc_types::AccountEncoding::Base64,
        ),
        owner: owner.to_string(),
        executable: false,
        rent_epoch: 0,
        space: data.len() as u64,
    };
    let pubkey = bs58::encode(address).into_string();
    let (_,): ((),) = update(
        &s.pic,
        s.rpc,
        "set_account",
        Encode!(&pubkey, &Some(account)).unwrap(),
    );
}

pub fn set_broken(s: &Setup, broken: bool) {
    let (_,): ((),) = update(&s.pic, s.rpc, "set_broken", Encode!(&broken).unwrap());
}

/// One prepared bid: an escrow account planted in the mock, ready to be
/// registered.
pub struct Bid {
    pub donor: Wallet,
    pub text_hash: [u8; 32],
    pub gross: u64,
    pub deadline: u64,
    pub nonce: u64,
    pub resolver: Vec<u8>,
    pub lot_id: [u8; 32],
    pub salt: [u8; 32],
    pub escrow: Vec<u8>,
}

/// Derives the bid's addresses, plants an honest escrow account into the
/// mock and returns everything registration needs.
#[allow(clippy::too_many_arguments)] // a birth is a birth: seven fields plus the harness
pub fn plant_bid(
    s: &Setup,
    auction_id: &[u8],
    km: &Wallet,
    donor_seed: u8,
    text_hash: [u8; 32],
    gross: u64,
    deadline: u64,
    nonce: u64,
) -> Bid {
    let donor = wallet(donor_seed);
    let resolver = get_resolver(s, auction_id, &text_hash).expect("resolver");
    let lot_id = auth::derive_lot_id(auction_id, &text_hash).unwrap();
    let salt = derive_salt(
        &donor.address,
        &km.address,
        gross,
        deadline,
        &resolver,
        nonce,
    );
    let escrow = derive_escrow_address(&salt);
    let data = escrow_account_data(
        &donor.address,
        &salt,
        &km.address,
        &resolver,
        gross,
        deadline,
        false,
    );
    plant_account(s, &escrow, FACTORY, &data);
    Bid {
        donor,
        text_hash,
        gross,
        deadline,
        nonce,
        resolver,
        lot_id,
        salt,
        escrow,
    }
}

pub fn register(
    s: &Setup,
    auction_id: &[u8],
    text_hash: &[u8],
    donor: &[u8],
    gross: u64,
    deadline: u64,
    nonce: u64,
) -> Result<Vec<u8>, String> {
    let arg = RegisterEntryArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        text_hash: ByteBuf::from(text_hash.to_vec()),
        donor: ByteBuf::from(donor.to_vec()),
        gross,
        deadline,
        nonce,
    };
    let (result,): (Result<ByteBuf, String>,) =
        update(&s.pic, s.game, "register_entry", Encode!(&arg).unwrap());
    result.map(|escrow| escrow.to_vec())
}

pub fn register_bid(s: &Setup, auction_id: &[u8], bid: &Bid) -> Result<Vec<u8>, String> {
    register(
        s,
        auction_id,
        &bid.text_hash,
        &bid.donor.address,
        bid.gross,
        bid.deadline,
        bid.nonce,
    )
}

fn lot_action(
    s: &Setup,
    method: &str,
    action: &auth::Action,
    auction_id: &[u8],
    lot_id: &[u8],
    signer: &Wallet,
) -> Result<(), String> {
    let message = auth::auction_message(CHAIN, &s.game.to_text(), auction_id, action);
    let arg = LotActionArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        lot_id: ByteBuf::from(lot_id.to_vec()),
        signature: ByteBuf::from(sign(signer, message.as_bytes())),
    };
    let (result,): (Result<(), String>,) = update(&s.pic, s.game, method, Encode!(&arg).unwrap());
    result
}

pub fn accept_lot(
    s: &Setup,
    auction_id: &[u8],
    lot_id: [u8; 32],
    signer: &Wallet,
) -> Result<(), String> {
    lot_action(
        s,
        "accept_lot",
        &auth::Action::Accept { lot: lot_id },
        auction_id,
        &lot_id,
        signer,
    )
}

pub fn return_lot(
    s: &Setup,
    auction_id: &[u8],
    lot_id: [u8; 32],
    signer: &Wallet,
) -> Result<(), String> {
    lot_action(
        s,
        "return_lot",
        &auth::Action::ReturnLot { lot: lot_id },
        auction_id,
        &lot_id,
        signer,
    )
}

pub fn return_entry(
    s: &Setup,
    auction_id: &[u8],
    escrow: &[u8],
    signer: &Wallet,
) -> Result<(), String> {
    let message = auth::auction_message(
        CHAIN,
        &s.game.to_text(),
        auction_id,
        &auth::Action::ReturnEntry {
            escrow: escrow.to_vec(),
        },
    );
    let arg = EntryActionArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        escrow: ByteBuf::from(escrow.to_vec()),
        signature: ByteBuf::from(sign(signer, message.as_bytes())),
    };
    let (result,): (Result<(), String>,) =
        update(&s.pic, s.game, "return_entry", Encode!(&arg).unwrap());
    result
}

pub fn cancel_auction(s: &Setup, auction_id: &[u8], signer: &Wallet) -> Result<(), String> {
    let message =
        auth::auction_message(CHAIN, &s.game.to_text(), auction_id, &auth::Action::Cancel);
    let arg = AuctionActionArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        signature: ByteBuf::from(sign(signer, message.as_bytes())),
    };
    let (result,): (Result<(), String>,) =
        update(&s.pic, s.game, "cancel_auction", Encode!(&arg).unwrap());
    result
}

pub fn ready(s: &Setup, auction_id: &[u8], signer: &Wallet) -> Result<(), String> {
    let message = auth::auction_message(CHAIN, &s.game.to_text(), auction_id, &auth::Action::Ready);
    let arg = AuctionActionArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        signature: ByteBuf::from(sign(signer, message.as_bytes())),
    };
    let (result,): (Result<(), String>,) = update(&s.pic, s.game, "ready", Encode!(&arg).unwrap());
    result
}

pub fn vote(
    s: &Setup,
    auction_id: &[u8],
    voter: &Wallet,
    choice: auction::ChoiceView,
) -> Result<(), String> {
    let auth_choice = match choice {
        auction::ChoiceView::Done => auth::Choice::Done,
        auction::ChoiceView::NotDone => auth::Choice::NotDone,
    };
    let message = auth::auction_message(
        CHAIN,
        &s.game.to_text(),
        auction_id,
        &auth::Action::Vote(auth_choice),
    );
    let arg = auction::api::VoteArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        voter: ByteBuf::from(voter.address.clone()),
        choice,
        signature: ByteBuf::from(sign(voter, message.as_bytes())),
    };
    let (result,): (Result<(), String>,) = update(&s.pic, s.game, "vote", Encode!(&arg).unwrap());
    result
}

pub fn operator_refund_lot(
    s: &Setup,
    auction_id: &[u8],
    lot_id: [u8; 32],
    signer: &Wallet,
) -> Result<(), String> {
    let message = auth::auction_message(
        CHAIN,
        &s.game.to_text(),
        auction_id,
        &auth::Action::OperatorRefundLot { lot: lot_id },
    );
    let arg = LotActionArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        lot_id: ByteBuf::from(lot_id.to_vec()),
        signature: ByteBuf::from(sign(signer, message.as_bytes())),
    };
    let (result,): (Result<(), String>,) = update(
        &s.pic,
        s.game,
        "operator_refund_lot",
        Encode!(&arg).unwrap(),
    );
    result
}

pub fn operator_refund_entry(
    s: &Setup,
    auction_id: &[u8],
    escrow: &[u8],
    signer: &Wallet,
) -> Result<(), String> {
    let message = auth::auction_message(
        CHAIN,
        &s.game.to_text(),
        auction_id,
        &auth::Action::OperatorRefundEntry {
            escrow: escrow.to_vec(),
        },
    );
    let arg = EntryActionArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        escrow: ByteBuf::from(escrow.to_vec()),
        signature: ByteBuf::from(sign(signer, message.as_bytes())),
    };
    let (result,): (Result<(), String>,) = update(
        &s.pic,
        s.game,
        "operator_refund_entry",
        Encode!(&arg).unwrap(),
    );
    result
}

pub fn request_signature(
    s: &Setup,
    auction_id: &[u8],
    bid: &Bid,
) -> Result<auction::api::SignedVerdict, String> {
    request_signature_raw(
        s,
        auction_id,
        &bid.text_hash,
        &bid.donor.address,
        bid.gross,
        bid.deadline,
        bid.nonce,
    )
}

pub fn request_signature_raw(
    s: &Setup,
    auction_id: &[u8],
    text_hash: &[u8],
    donor: &[u8],
    gross: u64,
    deadline: u64,
    nonce: u64,
) -> Result<auction::api::SignedVerdict, String> {
    let arg = auction::api::RequestSignatureArg {
        chain: CHAIN.to_string(),
        auction_id: ByteBuf::from(auction_id.to_vec()),
        text_hash: ByteBuf::from(text_hash.to_vec()),
        donor: ByteBuf::from(donor.to_vec()),
        gross,
        deadline,
        nonce,
    };
    let (result,): (Result<auction::api::SignedVerdict, String>,) =
        update(&s.pic, s.game, "request_signature", Encode!(&arg).unwrap());
    result
}

/// Verifies a verdict signature offchain exactly the way the escrow's
/// ed25519_program instruction would: Ed25519 over DOMAIN ‖ program ‖
/// escrow ‖ outcome against the lot's resolver.
pub fn verify_verdict(resolver: &[u8], escrow: &[u8], outcome: u8, signature: &[u8]) {
    let mut message = Vec::new();
    message.extend_from_slice(b"crown:two-outcome:solana-devnet");
    message.extend_from_slice(&bs58::decode(FACTORY).into_vec().unwrap());
    message.extend_from_slice(escrow);
    message.push(outcome);
    let key: [u8; 32] = resolver.try_into().expect("resolver is 32 bytes");
    let signature: [u8; 64] = signature.try_into().expect("signature is 64 bytes");
    ed25519_dalek::VerifyingKey::from_bytes(&key)
        .expect("resolver is a key")
        .verify_strict(&message, &ed25519_dalek::Signature::from_bytes(&signature))
        .expect("verdict signature verifies against the lot resolver");
}

/// Advances time and runs enough ticks for the global timer to drain due
/// work — including a chunked finale scan, whose slices re-arm a
/// near-immediate (+1s) tick: each round must move the clock past it.
pub fn advance(s: &Setup, secs: u64) {
    s.pic.advance_time(std::time::Duration::from_secs(secs));
    for _ in 0..12 {
        s.pic.advance_time(std::time::Duration::from_secs(2));
        s.pic.tick();
    }
}

// ---- reads -------------------------------------------------------------------

pub fn fetch_auction(s: &Setup, auction_id: &[u8]) -> CertifiedRecord {
    let (record,): (Option<CertifiedRecord>,) = query(
        &s.pic,
        s.game,
        "get_auction",
        Encode!(&CHAIN.to_string(), &ByteBuf::from(auction_id.to_vec())).unwrap(),
    );
    record.expect("auction exists")
}

pub fn auction_state(record: &CertifiedRecord) -> AuctionRecord {
    Decode!(record.data.as_slice(), AuctionRecord).expect("record decodes")
}

pub fn fetch_lot(s: &Setup, auction_id: &[u8], lot_id: &[u8]) -> CertifiedRecord {
    let (record,): (Option<CertifiedRecord>,) = query(
        &s.pic,
        s.game,
        "get_lot",
        Encode!(
            &CHAIN.to_string(),
            &ByteBuf::from(auction_id.to_vec()),
            &ByteBuf::from(lot_id.to_vec())
        )
        .unwrap(),
    );
    record.expect("lot exists")
}

pub fn lot_state(record: &CertifiedRecord) -> LotRecord {
    Decode!(record.data.as_slice(), LotRecord).expect("record decodes")
}

pub fn list_lots(s: &Setup, auction_id: &[u8]) -> Vec<LotRecord> {
    let (lots,): (Vec<LotRecord>,) = query(
        &s.pic,
        s.game,
        "list_lots",
        Encode!(&CHAIN.to_string(), &ByteBuf::from(auction_id.to_vec())).unwrap(),
    );
    lots
}

pub fn list_entries(s: &Setup, auction_id: &[u8], lot_id: &[u8]) -> Vec<EntryRecord> {
    let (entries,): (Vec<EntryRecord>,) = query(
        &s.pic,
        s.game,
        "list_entries",
        Encode!(
            &CHAIN.to_string(),
            &ByteBuf::from(auction_id.to_vec()),
            &ByteBuf::from(lot_id.to_vec())
        )
        .unwrap(),
    );
    entries
}

// ---- certificate verification --------------------------------------------------

/// Full offchain verification: BLS against the instance root key, the
/// certified_data binding, the witness path down to sha256(record bytes)
/// under the `auction` label at the given storage key.
pub fn verify_certified(s: &Setup, key: &[u8], record: &CertifiedRecord) {
    let certificate: Certificate =
        serde_cbor::from_slice(record.certificate.as_ref().expect("certificate present"))
            .expect("certificate decodes");
    assert!(
        certificate.delegation.is_none(),
        "NNS-subnet canister: no delegation"
    );

    // 1. Genuine: signed by the root key of the instance.
    let root_key = s.pic.root_key().expect("instance has a root key");
    let bls_key = &root_key[root_key.len() - 96..];
    let mut message = vec![13u8];
    message.extend_from_slice(b"ic-state-root");
    message.extend_from_slice(&certificate.tree.digest());
    ic_verify_bls_signature::verify_bls_signature(&certificate.signature, &message, bls_key)
        .expect("BLS signature verifies against the root key");

    // 2. Bound: the certificate certifies this canister's certified_data.
    let path = [b"canister".as_slice(), s.game.as_slice(), b"certified_data"];
    let LookupResult::Found(certified_data) = certificate.tree.lookup_path(&path) else {
        panic!("certified_data not in certificate");
    };

    // 3. Witnessed: the witness digest is the certified root, and its path
    // [auction, key] holds sha256 of the exact record bytes.
    let witness: HashTree = serde_cbor::from_slice(&record.witness).expect("witness decodes");
    assert_eq!(
        witness.digest().as_slice(),
        certified_data,
        "witness root == certified_data"
    );
    let LookupResult::Found(leaf) = witness.lookup_path([b"auction".as_slice(), key]) else {
        panic!("record key not witnessed");
    };
    let digest: [u8; 32] = Sha256::digest(record.data.as_slice()).into();
    assert_eq!(leaf, digest, "witness pins the returned bytes");
}

pub fn verify_certified_auction(s: &Setup, auction_id: &[u8], record: &CertifiedRecord) {
    verify_certified(s, &auction_key(CHAIN, auction_id), record);
}
