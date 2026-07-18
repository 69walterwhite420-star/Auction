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

/// True when the lot is the recorded winner of the auction.
fn is_winner(record: &crate::AuctionRecord, lot_id: &[u8]) -> bool {
    record.winner_lot.as_ref().map(|w| w.as_slice()) == Some(lot_id)
}

/// The KM returns a whole lot: cancel for every entry it has and will ever
/// derive, and no further registrations (docs/game-spec.md §5). In BIDDING
/// any lot; after the finale only the winner, until "ready" — returning it
/// kills the auction unsettled, there is no second place.
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
    let action = if is_winner(&record, &lot) {
        logic::Action::KmReturnWinner
    } else {
        logic::Action::ReturnLot
    };
    step_and_save(&akey, &mut record, action, now)?;

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

/// The platform operator returns a whole lot — the censorship move
/// (docs/game-spec.md §13). In BIDDING any lot; after the finale only the
/// winner, from PERFORMING and VOTING alike; a decided lot is out of reach.
/// The only direction is the bidders' own money back; settle has no such
/// door. Attributed forever.
#[ic_cdk::update]
fn operator_refund_lot(arg: LotActionArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;
    let lot: [u8; 32] = arg
        .lot_id
        .to_vec()
        .try_into()
        .map_err(|_| "bad field length")?;

    let operator = crate::operator_wallet().ok_or("no operator wallet configured")?;
    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::OperatorRefundLot { lot },
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &operator)
        .map_err(|e| e.text().to_string())?;

    let now = crate::now_seconds();
    let winner = is_winner(&record, &lot);
    let action = if winner {
        logic::Action::OperatorReturnWinner
    } else {
        logic::Action::ReturnLot
    };
    // Manual step: the auction-level attribution must land in the same
    // certified write as the state change.
    let before = record.clone();
    let mut auction = record.to_logic();
    let result = logic::step(&mut auction, action, now);
    record.absorb(&auction);
    if result.is_ok() && winner {
        record.operator_returned_at = Some(now);
    }
    if record != before {
        crate::save_auction(&akey, &record);
    }
    result.map_err(step_error_text)?;

    let lkey = crate::lot_key(&arg.chain, &arg.auction_id, &lot);
    let mut lot = crate::load_lot(&lkey).ok_or_else(|| "unknown lot".to_string())?;
    if lot.returned.is_some() {
        return Err("lot already returned".to_string());
    }
    lot.returned = Some(crate::ReturnStamp {
        at: now,
        by: crate::ActorView::Operator,
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
    refund_entry(arg, Refunder::Km)
}

/// The platform operator returns one entry (docs/game-spec.md §13): its
/// escrow gets cancel, the lot stays in the race with the rest. Available
/// wherever the machine draws it — any lot in BIDDING, the winner's entries
/// through PERFORMING and VOTING.
#[ic_cdk::update]
fn operator_refund_entry(arg: EntryActionArg) -> Result<(), String> {
    refund_entry(arg, Refunder::Operator)
}

/// Who returns the entry: the two update methods differ only in the signer,
/// the signed action word and the attribution tag.
enum Refunder {
    Km,
    Operator,
}

fn refund_entry(arg: EntryActionArg, who: Refunder) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    let (signer, action, actor, view) = match who {
        Refunder::Km => (
            record.km.to_vec(),
            auth::Action::ReturnEntry {
                escrow: arg.escrow.to_vec(),
            },
            logic::Actor::Km,
            crate::ActorView::Km,
        ),
        Refunder::Operator => (
            crate::operator_wallet()
                .ok_or("no operator wallet configured")?
                .to_vec(),
            auth::Action::OperatorRefundEntry {
                escrow: arg.escrow.to_vec(),
            },
            logic::Actor::Operator,
            crate::ActorView::Operator,
        ),
    };

    let message = auth::auction_message(&arg.chain, &canister_text(), &arg.auction_id, &action);
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &signer)
        .map_err(|e| e.text().to_string())?;

    let lot_id = crate::lot_of_entry(&arg.chain, &arg.auction_id, &arg.escrow)
        .ok_or_else(|| "unknown entry".to_string())?;
    let in_winner_lot = is_winner(&record, &lot_id);

    let now = crate::now_seconds();
    step_and_save(
        &akey,
        &mut record,
        logic::Action::ReturnEntry {
            by: actor,
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
    entry.returned = Some(crate::ReturnStamp { at: now, by: view });
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

/// The platform operator kills running bidding altogether — the censorship
/// move against a stream of junk lots that per-lot returns would chase one
/// by one (docs/game-spec.md §5, §13). BIDDING only: after the finale the
/// operator kills an auction by returning its winner. Every lot, known and
/// never registered alike, resolves to cancel by the auction rule.
/// Attributed forever.
#[ic_cdk::update]
fn operator_cancel_auction(arg: AuctionActionArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    let operator = crate::operator_wallet().ok_or("no operator wallet configured")?;
    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::OperatorCancel,
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &operator)
        .map_err(|e| e.text().to_string())?;

    // Manual step: the attribution must land in the same certified write as
    // the state change.
    let now = crate::now_seconds();
    let before = record.clone();
    let mut auction = record.to_logic();
    let result = logic::step(&mut auction, logic::Action::OperatorCancel, now);
    record.absorb(&auction);
    if result.is_ok() {
        record.operator_returned_at = Some(now);
    }
    if record != before {
        crate::save_auction(&akey, &record);
    }
    result.map_err(step_error_text)
}

/// The KM claims the winning condition performed: PERFORMING → VOTING, the
/// work goes to the community's judgment and the KM's return door closes
/// (docs/game-spec.md §5).
#[ic_cdk::update]
fn ready(arg: AuctionActionArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::Ready,
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &record.km)
        .map_err(|e| e.text().to_string())?;

    step_and_save(
        &akey,
        &mut record,
        logic::Action::Ready,
        crate::now_seconds(),
    )
}

#[derive(CandidType, Deserialize)]
pub struct VoteArg {
    pub chain: String,
    pub auction_id: ByteBuf,
    pub voter: ByteBuf,
    pub choice: crate::ChoiceView,
    pub signature: ByteBuf,
}

/// One vote on the winner (docs/game-spec.md §10). Order: signature, time,
/// dedup, then the paid weight call; the machine revalidates everything
/// after the await — the voting window may have closed while the book was
/// answering.
#[ic_cdk::update]
async fn vote(arg: VoteArg) -> Result<(), String> {
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    // Authorize before touching state: a bogus signature must never cost a
    // certified write.
    let choice = match arg.choice {
        crate::ChoiceView::Done => auth::Choice::Done,
        crate::ChoiceView::NotDone => auth::Choice::NotDone,
    };
    let message = auth::auction_message(
        &arg.chain,
        &canister_text(),
        &arg.auction_id,
        &auth::Action::Vote(choice),
    );
    auth::verify_wallet_signature(message.as_bytes(), &arg.signature, &arg.voter)
        .map_err(|e| e.text().to_string())?;

    // Time first, persisted (step_and_save writes only real transitions).
    step_and_save(
        &akey,
        &mut record,
        logic::Action::Tick,
        crate::now_seconds(),
    )?;
    if !matches!(record.state, crate::StateView::Voting { .. }) {
        return Err("invalid transition".to_string());
    }

    // Dedup before paying for the book call; the machine dedups again after.
    if record.votes.iter().any(|vote| vote.voter == arg.voter) {
        return Err("duplicate voter".to_string());
    }

    let weight = crate::weight::book_value(&arg.chain, &arg.voter, &record.km).await?;

    // The await yielded: reload the truth and let the machine judge.
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;
    step_and_save(
        &akey,
        &mut record,
        logic::Action::Vote(logic::Vote {
            voter: logic::Voter(arg.voter.to_vec()),
            choice: match arg.choice {
                crate::ChoiceView::Done => logic::Choice::Done,
                crate::ChoiceView::NotDone => logic::Choice::NotDone,
            },
            weight,
        }),
        crate::now_seconds(),
    )
}

#[derive(CandidType, Deserialize)]
pub struct RequestSignatureArg {
    pub chain: String,
    pub auction_id: ByteBuf,
    pub text_hash: ByteBuf,
    pub donor: ByteBuf,
    pub gross: u64,
    pub deadline: u64,
    pub nonce: u64,
}

#[derive(CandidType, Deserialize, Debug)]
pub struct SignedVerdict {
    pub escrow: ByteBuf,
    pub outcome: crate::OutcomeView,
    pub signature: ByteBuf,
}

/// The three-step outcome of one escrow (game-spec §8): its own return →
/// its lot's return → the auction rule. A lot the registry never heard of
/// resolves by the auction rule alone: unregistered means unaccepted means
/// lost.
fn resolve_outcome(
    record: &crate::AuctionRecord,
    lot: &Option<crate::LotRecord>,
    entry: &Option<crate::EntryRecord>,
    lot_id: &[u8],
) -> Result<crate::OutcomeView, String> {
    if let Some(entry) = entry
        && entry.returned.is_some()
    {
        return Ok(crate::OutcomeView::Cancel);
    }
    if let Some(lot) = lot
        && lot.returned.is_some()
    {
        return Ok(crate::OutcomeView::Cancel);
    }
    let winner = is_winner(record, lot_id);
    match record.state {
        crate::StateView::Done { winner: outcome } => {
            if winner {
                outcome.ok_or_else(|| "no verdict yet".to_string())
            } else {
                Ok(crate::OutcomeView::Cancel)
            }
        }
        // Losers are freed the moment the finale stands (game-spec §5);
        // the winner waits for its own verdict.
        crate::StateView::Performing | crate::StateView::Voting { .. } => {
            if winner {
                Err("no verdict yet".to_string())
            } else {
                Ok(crate::OutcomeView::Cancel)
            }
        }
        crate::StateView::Bidding | crate::StateView::FinaleDue => {
            Err("no verdict yet".to_string())
        }
    }
}

/// The signature on demand (docs/game-spec.md §8). Permissionless: the
/// right is the derivation arithmetic — `km` comes from the record, the
/// resolver from [lot_id], the fee from config; a foreign declaration
/// derives an address where no escrow exists, and a signature for it is
/// harmless. Nothing is stored: a retry re-signs the same recorded
/// resolution, and two outcomes for one escrow never exist.
#[ic_cdk::update]
async fn request_signature(arg: RequestSignatureArg) -> Result<SignedVerdict, String> {
    let spec = auth::spec_of(&arg.chain).map_err(|e| e.text().to_string())?;
    let akey = crate::auction_key(&arg.chain, &arg.auction_id);
    let mut record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;

    // Time first: a due tally must not wait for the global timer.
    step_and_save(
        &akey,
        &mut record,
        logic::Action::Tick,
        crate::now_seconds(),
    )?;

    let lot_id =
        auth::derive_lot_id(&arg.auction_id, &arg.text_hash).map_err(|e| e.text().to_string())?;
    let lkey = crate::lot_key(&arg.chain, &arg.auction_id, &lot_id);
    let resolver = match crate::load_lot(&lkey) {
        Some(lot) => lot.resolver.to_vec(),
        None => crate::sign::lot_resolver(&lot_id).await?,
    };
    let (escrow, _salt) = auth::derive_escrow(
        spec,
        &arg.donor,
        &record.km,
        arg.gross,
        arg.deadline,
        &resolver,
        arg.nonce,
    )
    .map_err(|e| e.text().to_string())?;

    // The await may have yielded: resolve against the freshest truth.
    let record = crate::load_auction(&akey).ok_or_else(|| "unknown auction".to_string())?;
    let lot = crate::load_lot(&lkey);
    let entry = crate::load_entry(&crate::entry_key(
        &arg.chain,
        &arg.auction_id,
        &lot_id,
        &escrow,
    ));
    let outcome = resolve_outcome(&record, &lot, &entry, &lot_id)?;

    // The contract outcome index of the resolution (factory-spec §2.2).
    let outcome_byte = match outcome {
        crate::OutcomeView::Settle => 0u8,
        crate::OutcomeView::Cancel => 1u8,
    };
    let program = bs58::decode(spec.factory)
        .into_vec()
        .map_err(|_| "malformed factory program id")?;
    let message = crate::sign::verdict_message(spec.domain, &program, &escrow, outcome_byte);
    let signature = crate::sign::sign_verdict(&lot_id, &resolver, &message).await?;
    Ok(SignedVerdict {
        escrow: ByteBuf::from(escrow),
        outcome,
        signature: ByteBuf::from(signature),
    })
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use serde_bytes::ByteBuf;

    use super::*;

    const WINNER_LOT: [u8; 32] = [0xB1; 32];
    const OTHER_LOT: [u8; 32] = [0xC2; 32];

    fn record(state: crate::StateView, winner_lot: Option<[u8; 32]>) -> crate::AuctionRecord {
        crate::AuctionRecord {
            chain: "solana-devnet".to_string(),
            auction_id: ByteBuf::from(vec![0xAA; 32]),
            km: ByteBuf::from(vec![0x22; 32]),
            km_nonce: 1,
            created_at: 0,
            duration: 60,
            perform_window: 60,
            voting_period: 60,
            min_bid: 0,
            state,
            votes: Vec::new(),
            winner_lot: winner_lot.map(|w| ByteBuf::from(w.to_vec())),
            operator_returned_at: None,
        }
    }

    fn lot(returned: bool) -> crate::LotRecord {
        crate::LotRecord {
            lot_id: ByteBuf::from(WINNER_LOT.to_vec()),
            text_hash: ByteBuf::from(vec![0x01; 32]),
            resolver: ByteBuf::from(vec![0x02; 32]),
            accepted_at: Some(1),
            returned: returned.then_some(crate::ReturnStamp {
                at: 2,
                by: crate::ActorView::Km,
            }),
            sum: 1,
            entries: 1,
        }
    }

    fn entry(returned: bool) -> crate::EntryRecord {
        crate::EntryRecord {
            escrow: ByteBuf::from(vec![0x03; 32]),
            lot_id: ByteBuf::from(WINNER_LOT.to_vec()),
            donor: ByteBuf::from(vec![0x04; 32]),
            gross: 1,
            deadline: 9,
            nonce: 0,
            seq: 1,
            returned: returned.then_some(crate::ReturnStamp {
                at: 2,
                by: crate::ActorView::Operator,
            }),
        }
    }

    /// The three-step resolution, exhaustively: every state × winner/loser
    /// × lot return × entry return. An entry's own return wins over
    /// everything; a lot's return wins over the auction rule; the auction
    /// rule frees losers the moment the finale stands and makes the winner
    /// wait for its own verdict (game-spec §8).
    #[test]
    fn outcome_resolution_is_exhaustive() {
        use crate::OutcomeView::{Cancel, Settle};
        let states = [
            crate::StateView::Bidding,
            crate::StateView::FinaleDue,
            crate::StateView::Performing,
            crate::StateView::Voting { started_at: 1 },
            crate::StateView::Done { winner: None },
            crate::StateView::Done {
                winner: Some(Settle),
            },
            crate::StateView::Done {
                winner: Some(Cancel),
            },
        ];
        for state in states {
            for queried_lot in [WINNER_LOT, OTHER_LOT] {
                for lot_returned in [false, true] {
                    for entry_returned in [false, true] {
                        // The finale names a winner only where one exists.
                        let finale_ran = !matches!(
                            state,
                            crate::StateView::Bidding
                                | crate::StateView::FinaleDue
                                | crate::StateView::Done { winner: None }
                        );
                        let record = record(state.clone(), finale_ran.then_some(WINNER_LOT));
                        let is_winner = finale_ran && queried_lot == WINNER_LOT;
                        let got = resolve_outcome(
                            &record,
                            &Some(lot(lot_returned)),
                            &Some(entry(entry_returned)),
                            &queried_lot,
                        );
                        let expected = if entry_returned || lot_returned {
                            Ok(Cancel)
                        } else {
                            match (&state, is_winner) {
                                (crate::StateView::Done { winner }, true) => {
                                    winner.ok_or_else(|| "no verdict yet".to_string())
                                }
                                (crate::StateView::Done { .. }, false) => Ok(Cancel),
                                (
                                    crate::StateView::Performing | crate::StateView::Voting { .. },
                                    true,
                                ) => Err("no verdict yet".to_string()),
                                (
                                    crate::StateView::Performing | crate::StateView::Voting { .. },
                                    false,
                                ) => Ok(Cancel),
                                _ => Err("no verdict yet".to_string()),
                            }
                        };
                        assert_eq!(got, expected, "state {state:?} winner {is_winner}");

                        // A lot the registry never heard of resolves by the
                        // auction rule alone.
                        let got = resolve_outcome(&record, &None, &None, &queried_lot);
                        let expected = match (&state, is_winner) {
                            (crate::StateView::Done { winner }, true) => {
                                winner.ok_or_else(|| "no verdict yet".to_string())
                            }
                            (crate::StateView::Done { .. }, false) => Ok(Cancel),
                            (
                                crate::StateView::Performing | crate::StateView::Voting { .. },
                                true,
                            ) => Err("no verdict yet".to_string()),
                            (
                                crate::StateView::Performing | crate::StateView::Voting { .. },
                                false,
                            ) => Ok(Cancel),
                            _ => Err("no verdict yet".to_string()),
                        };
                        assert_eq!(got, expected, "unregistered lot, state {state:?}");
                    }
                }
            }
        }
    }
}
