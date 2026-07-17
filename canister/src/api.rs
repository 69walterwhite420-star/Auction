//! The Candid surface. Updates are exactly the frozen allowlist; every
//! signed one authorizes by wallet signature, never by principal, and the
//! permissionless ones — `get_resolver`, `register_entry` — carry their
//! right in derivation arithmetic and the chain read. Queries are free,
//! permissionless; `get_auction` and `get_lot` carry certificate + witness.

use auction_logic as logic;
use candid::CandidType;
use serde::Deserialize;
use serde_bytes::ByteBuf;

use crate::auth;

fn step_error_text(error: logic::StepError) -> String {
    match error {
        logic::StepError::InvalidTransition => "invalid transition",
        logic::StepError::BelowMinBid => "gross below the auction's min_bid",
        logic::StepError::DeadlineTooShort => "escrow deadline too short",
        logic::StepError::WeightBelowThreshold => "vote weight below threshold",
        logic::StepError::DuplicateVoter => "duplicate voter",
        logic::StepError::Overflow => "arithmetic overflow",
    }
    .to_string()
}

fn create_error_text(error: logic::CreateError) -> String {
    match error {
        logic::CreateError::DurationOutOfRange => "duration out of range",
        logic::CreateError::PerformWindowOutOfRange => "perform window out of range",
    }
    .to_string()
}

/// Raw principal bytes — the auction_id hashes these, and that derivation
/// must not move.
fn canister_id() -> Vec<u8> {
    ic_cdk::api::canister_self().as_slice().to_vec()
}

/// The same principal in text form, as the signed message shows it.
fn canister_text() -> String {
    ic_cdk::api::canister_self().to_text()
}

fn profile() -> logic::Profile {
    logic::Profile {
        voting_period: crate::VOTING_PERIOD,
    }
}

/// Applies one machine action to a loaded record and persists the result
/// when anything changed — due time transitions persist even when the
/// action itself fails, an unchanged step never costs a certified write.
fn step_and_save(
    key: &[u8],
    record: &mut crate::AuctionRecord,
    action: logic::Action,
    now: u64,
) -> Result<(), String> {
    let before = record.clone();
    let mut auction = record.to_logic();
    let result = logic::step(&mut auction, action, now);
    record.absorb(&auction);
    if *record != before {
        crate::save_auction(key, record);
    }
    result.map_err(step_error_text)
}

// ---- updates -----------------------------------------------------------------

#[derive(CandidType, Deserialize)]
pub struct CreateAuctionArg {
    pub chain: String,
    pub km: ByteBuf,
    pub km_nonce: u64,
    pub duration: u64,
    pub perform_window: u64,
    pub min_bid: u64,
    pub signature: ByteBuf,
}

/// Opens an auction: derives auction_id from (canister, km, km_nonce),
/// checks the KM's signature over the handles and births the machine in
/// BIDDING (docs/game-spec.md §5, §11). Synchronous — no key derivation
/// happens here: lots derive their own resolvers when they appear.
#[ic_cdk::update]
fn create_auction(arg: CreateAuctionArg) -> Result<ByteBuf, String> {
    auth::spec_of(&arg.chain).map_err(|e| e.text().to_string())?;
    let auction_id = auth::derive_auction_id(&canister_id(), &arg.km, arg.km_nonce)
        .map_err(|e| e.text().to_string())?;
    let key = crate::auction_key(&arg.chain, &auction_id);
    if crate::load_auction_bytes(&key).is_some() {
        return Err("auction already exists".to_string());
    }

    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &auction_id,
        &auth::Action::Create {
            km_nonce: arg.km_nonce,
            duration: arg.duration,
            perform_window: arg.perform_window,
            min_bid: arg.min_bid,
        },
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &arg.km)
        .map_err(|e| e.text().to_string())?;

    let auction = logic::create(
        crate::now_seconds(),
        &profile(),
        arg.duration,
        arg.perform_window,
        arg.min_bid,
    )
    .map_err(create_error_text)?;

    let mut record = crate::AuctionRecord {
        chain: arg.chain,
        auction_id: ByteBuf::from(auction_id.to_vec()),
        km: arg.km,
        km_nonce: arg.km_nonce,
        created_at: auction.created_at,
        duration: auction.duration,
        perform_window: auction.perform_window,
        voting_period: auction.voting_period,
        min_bid: auction.min_bid,
        state: crate::state_to_view(&auction.state),
        votes: Vec::new(),
        winner_lot: None,
        operator_returned_at: None,
    };
    record.absorb(&auction);
    crate::save_auction(&key, &record);
    Ok(ByteBuf::from(auction_id.to_vec()))
}

#[derive(CandidType, Deserialize)]
pub struct GetResolverArg {
    pub auction_id: ByteBuf,
    pub text_hash: ByteBuf,
}

/// The RESOLVER birth field for escrows of one lot: the derived Ed25519
/// pubkey at path [lot_id]. Permissionless and stateless (the Subscription
/// pattern): a bidder needs it before the lot exists anywhere. An update,
/// not a query — threshold derivation is an async management call.
#[ic_cdk::update]
async fn get_resolver(arg: GetResolverArg) -> Result<ByteBuf, String> {
    let lot_id =
        auth::derive_lot_id(&arg.auction_id, &arg.text_hash).map_err(|e| e.text().to_string())?;
    let resolver = crate::sign::lot_resolver(&lot_id).await?;
    Ok(ByteBuf::from(resolver))
}

#[derive(CandidType, Deserialize)]
pub struct RegisterEntryArg {
    pub chain: String,
    pub auction_id: ByteBuf,
    pub text_hash: ByteBuf,
    pub donor: ByteBuf,
    pub gross: u64,
    pub deadline: u64,
    pub nonce: u64,
}

/// Registers one entry: derivation rebuilds the escrow address from the
/// declared birth fields (km from the record, resolver from [lot_id], the
/// game's fee from config), the chain read confirms the account is real,
/// and only then the entry joins the leaderboard with the next seq
/// (docs/game-spec.md §4, §8). Permissionless: registering someone else's
/// escrow only helps its donor.
#[ic_cdk::update]
async fn register_entry(arg: RegisterEntryArg) -> Result<ByteBuf, String> {
    let spec = auth::spec_of(&arg.chain).map_err(|e| e.text().to_string())?;
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    // The state, floor and deadline law — cheap and before any paid call.
    // Time first: an expired bidding window persists as FINALE_DUE even
    // though the registration is rejected.
    let register = logic::Action::Register {
        gross: arg.gross,
        deadline: i64::try_from(arg.deadline).map_err(|_| "escrow deadline too short")?,
    };
    step_and_save(&akey, &mut record, register.clone(), crate::now_seconds())?;

    let lot_id =
        auth::derive_lot_id(&arg.auction_id, &arg.text_hash).map_err(|e| e.text().to_string())?;
    let lkey = crate::lot_key(&arg.chain, &arg.auction_id, &lot_id);
    let known_lot = crate::load_lot(&lkey);
    if let Some(lot) = &known_lot
        && lot.returned.is_some()
    {
        return Err("lot already returned".to_string());
    }
    // First entry of a lot derives the lot's resolver; later ones reuse the
    // stored key (the derivation is deterministic — same value either way).
    let resolver = match &known_lot {
        Some(lot) => lot.resolver.to_vec(),
        None => crate::sign::lot_resolver(&lot_id).await?,
    };

    let (escrow, salt) = auth::derive_escrow(
        spec,
        &arg.donor,
        &record.km,
        arg.gross,
        arg.deadline,
        &resolver,
        arg.nonce,
    )
    .map_err(|e| e.text().to_string())?;

    let ekey = crate::entry_key(&arg.chain, &arg.auction_id, &lot_id, &escrow);
    if crate::load_entry(&ekey).is_some() {
        return Err("entry already registered".to_string());
    }

    // The read (game-spec §4): the leaderboard admits facts of the chain
    // only. A transport error aborts the call; nothing was written.
    crate::solana::verify_escrow(spec, &escrow, &arg.donor, &salt).await?;

    // The awaits yielded execution: reload the truth and re-apply the law
    // before writing. The clock may have closed bidding meanwhile, the lot
    // may have been returned, the entry may have raced in.
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;
    step_and_save(&akey, &mut record, register, crate::now_seconds())?;
    let mut lot = match crate::load_lot(&lkey) {
        Some(lot) if lot.returned.is_some() => {
            return Err("lot already returned".to_string());
        }
        Some(lot) => lot,
        None => crate::LotRecord {
            lot_id: ByteBuf::from(lot_id.to_vec()),
            text_hash: arg.text_hash.clone(),
            resolver: ByteBuf::from(resolver),
            accepted_at: None,
            returned: None,
            sum: 0,
            entries: 0,
        },
    };
    if crate::load_entry(&ekey).is_some() {
        return Err("entry already registered".to_string());
    }

    let entry = crate::EntryRecord {
        escrow: ByteBuf::from(escrow.clone()),
        lot_id: ByteBuf::from(lot_id.to_vec()),
        donor: arg.donor,
        gross: arg.gross,
        deadline: arg.deadline,
        nonce: arg.nonce,
        seq: crate::next_seq(),
        returned: None,
    };
    lot.sum = lot
        .sum
        .checked_add(u128::from(arg.gross))
        .ok_or("lot sum overflow")?;
    lot.entries = lot.entries.saturating_add(1);
    crate::save_entry(&ekey, &entry);
    crate::index_entry(&arg.chain, &arg.auction_id, &escrow, &lot_id);
    crate::save_lot(&lkey, &lot);
    Ok(ByteBuf::from(escrow))
}

#[derive(CandidType, Deserialize)]
pub struct LotActionArg {
    pub chain: String,
    pub auction_id: ByteBuf,
    pub lot_id: ByteBuf,
    pub signature: ByteBuf,
}

/// The KM takes a lot into the race: its text goes public — the server sees
/// the certified state change and publishes text + text_salt, anyone checks
/// the hash (docs/game-spec.md §7). BIDDING only, until the last second.
#[ic_cdk::update]
fn accept_lot(arg: LotActionArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;
    let lot: [u8; 32] = arg
        .lot_id
        .to_vec()
        .try_into()
        .map_err(|_| "bad field length")?;

    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::Accept { lot },
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &record.km)
        .map_err(|e| e.text().to_string())?;

    let now = crate::now_seconds();
    step_and_save(&akey, &mut record, logic::Action::AcceptLot, now)?;

    let lkey = crate::lot_key(&arg.chain, &arg.auction_id, &lot);
    let mut lot = crate::load_lot(&lkey).ok_or_else(|| "unknown lot".to_string())?;
    if lot.returned.is_some() {
        return Err("lot already returned".to_string());
    }
    if lot.accepted_at.is_some() {
        return Err("lot already accepted".to_string());
    }
    lot.accepted_at = Some(now);
    crate::save_lot(&lkey, &lot);
    Ok(())
}

/// The KM returns a whole lot: cancel for every entry it has and will ever
/// derive, and no further registrations (docs/game-spec.md §5). BIDDING
/// only — the winner goes through its own doors (G3).
#[ic_cdk::update]
fn return_lot(arg: LotActionArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;
    let lot: [u8; 32] = arg
        .lot_id
        .to_vec()
        .try_into()
        .map_err(|_| "bad field length")?;

    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::ReturnLot { lot },
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &record.km)
        .map_err(|e| e.text().to_string())?;

    let now = crate::now_seconds();
    step_and_save(&akey, &mut record, logic::Action::ReturnLot, now)?;

    let lkey = crate::lot_key(&arg.chain, &arg.auction_id, &lot);
    let mut lot = crate::load_lot(&lkey).ok_or_else(|| "unknown lot".to_string())?;
    if lot.returned.is_some() {
        return Err("lot already returned".to_string());
    }
    lot.returned = Some(crate::ReturnStamp {
        at: now,
        by: crate::ActorView::Km,
    });
    crate::save_lot(&lkey, &lot);
    Ok(())
}

#[derive(CandidType, Deserialize)]
pub struct EntryActionArg {
    pub chain: String,
    pub auction_id: ByteBuf,
    pub escrow: ByteBuf,
    pub signature: ByteBuf,
}

/// The KM returns one entry: its escrow gets cancel, the lot stays in the
/// race with the rest (docs/game-spec.md §5). The sum and the seq
/// resolution of the lot lose the entry.
#[ic_cdk::update]
fn return_entry(arg: EntryActionArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::ReturnEntry {
            escrow: arg.escrow.to_vec(),
        },
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &record.km)
        .map_err(|e| e.text().to_string())?;

    let lot_id = crate::lot_of_entry(&arg.chain, &arg.auction_id, &arg.escrow)
        .ok_or_else(|| "unknown entry".to_string())?;
    let in_winner_lot = record.winner_lot.as_ref().map(|w| w.to_vec()) == Some(lot_id.clone());

    let now = crate::now_seconds();
    step_and_save(
        &akey,
        &mut record,
        logic::Action::ReturnEntry {
            by: logic::Actor::Km,
            in_winner_lot,
        },
        now,
    )?;

    let lkey = crate::lot_key(&arg.chain, &arg.auction_id, &lot_id);
    let mut lot = crate::load_lot(&lkey).ok_or_else(|| "unknown lot".to_string())?;
    if lot.returned.is_some() {
        return Err("lot already returned".to_string());
    }
    let ekey = crate::entry_key(&arg.chain, &arg.auction_id, &lot_id, &arg.escrow);
    let mut entry = crate::load_entry(&ekey).ok_or_else(|| "unknown entry".to_string())?;
    if entry.returned.is_some() {
        return Err("entry already returned".to_string());
    }
    entry.returned = Some(crate::ReturnStamp {
        at: now,
        by: crate::ActorView::Km,
    });
    lot.sum = lot
        .sum
        .checked_sub(u128::from(entry.gross))
        .ok_or("lot sum underflow")?;
    crate::save_entry(&ekey, &entry);
    crate::save_lot(&lkey, &lot);
    Ok(())
}

#[derive(CandidType, Deserialize)]
pub struct AuctionActionArg {
    pub chain: String,
    pub auction_id: ByteBuf,
    pub signature: ByteBuf,
}

/// The KM aborts the whole auction: cancel for every lot, known and never
/// registered alike — the auction-level rule resolves them all (game-spec
/// §8). BIDDING only.
#[ic_cdk::update]
fn cancel_auction(arg: AuctionActionArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::Cancel,
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &record.km)
        .map_err(|e| e.text().to_string())?;

    step_and_save(
        &akey,
        &mut record,
        logic::Action::KmCancel,
        crate::now_seconds(),
    )
}

// ---- queries -----------------------------------------------------------------

#[derive(CandidType, Deserialize)]
pub struct CertifiedRecord {
    /// The exact stored candid bytes of the record; the witness pins their
    /// sha256, the certificate signs the witness root.
    pub data: ByteBuf,
    pub certificate: Option<ByteBuf>,
    pub witness: ByteBuf,
}

fn certified(key: &[u8], data: Vec<u8>) -> CertifiedRecord {
    CertifiedRecord {
        data: ByteBuf::from(data),
        certificate: ic_cdk::api::data_certificate().map(ByteBuf::from),
        witness: ByteBuf::from(crate::certify::witness(key)),
    }
}

#[ic_cdk::query]
fn get_auction(chain: String, auction_id: ByteBuf) -> Option<CertifiedRecord> {
    let key = crate::auction_key(&chain, &auction_id);
    Some(certified(&key, crate::load_auction_bytes(&key)?))
}

#[ic_cdk::query]
fn get_lot(chain: String, auction_id: ByteBuf, lot_id: ByteBuf) -> Option<CertifiedRecord> {
    let key = crate::lot_key(&chain, &auction_id, &lot_id);
    Some(certified(&key, crate::load_lot_bytes(&key)?))
}

/// The leaderboard: every lot of one auction, uncertified (certify one lot
/// with `get_lot`). A full prefix scan — the accepted listing limitation
/// (game-spec §15).
#[ic_cdk::query]
fn list_lots(chain: String, auction_id: ByteBuf) -> Vec<crate::LotRecord> {
    crate::lots_of_auction(&chain, &auction_id)
}

#[ic_cdk::query]
fn list_entries(chain: String, auction_id: ByteBuf, lot_id: ByteBuf) -> Vec<crate::EntryRecord> {
    crate::entries_of_lot(&chain, &auction_id, &lot_id)
}

#[ic_cdk::query]
fn get_logic_version() -> u32 {
    logic::LOGIC_VERSION
}
