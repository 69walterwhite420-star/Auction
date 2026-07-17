//! auction canister: auctions, lots, chain-read registration, verdict
//! signatures (docs/game-spec.md).
//!
//! The update surface is frozen by the .did allowlist lint. Authorization is
//! a wallet signature, never a principal. The canister moves no money; its
//! single view of the outside world is one account read per registration,
//! through the SOL RPC canister named in config. The registry is the
//! leaderboard, never the source of the right to sign — that is derivation.

pub mod api;
pub mod auth;
pub mod certify;
pub mod sign;
pub mod solana;
pub mod weight;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use auction_logic as logic;
use candid::{CandidType, Decode, Encode};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap, StableCell};
use serde::Deserialize;

/// One chain the game serves; baked from config/ at build time.
pub struct ChainSpec {
    pub id: &'static str,
    pub factory: &'static str,
    /// Cluster-scoped verdict domain, part of the signed message.
    pub domain: &'static str,
    /// The read side of the game (game-spec §4): SOL RPC source variant and
    /// provider consensus strategy. Custom sources are testnet-only.
    pub source: &'static str,
    pub consensus: &'static str,
    /// The game's price tag: birth fields of every escrow it recognizes.
    /// An escrow born with a different fee derives a different salt and is
    /// simply never part of a lot.
    pub fee_bps: u16,
    pub fee_wallet: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/profile.rs"));

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

pub(crate) const AUCTIONS_MEMORY: MemoryId = MemoryId::new(0);
pub(crate) const LOTS_MEMORY: MemoryId = MemoryId::new(1);
pub(crate) const ENTRIES_MEMORY: MemoryId = MemoryId::new(2);
pub(crate) const ENTRY_LOT_MEMORY: MemoryId = MemoryId::new(3);
pub(crate) const SEQ_MEMORY: MemoryId = MemoryId::new(4);
pub(crate) const SOL_RPC_MEMORY: MemoryId = MemoryId::new(5);
pub(crate) const CROWN_INDEX_MEMORY: MemoryId = MemoryId::new(6);
pub(crate) const OPERATOR_WALLET_MEMORY: MemoryId = MemoryId::new(7);

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));

    /// Stored candid bytes of AuctionRecord, keyed by lp(chain)‖lp(auction_id).
    static AUCTIONS: RefCell<StableBTreeMap<Vec<u8>, Vec<u8>, Memory>> =
        RefCell::new(StableBTreeMap::init(memory(AUCTIONS_MEMORY)));

    /// Stored candid bytes of LotRecord, keyed by
    /// lp(chain)‖lp(auction_id)‖lp(lot_id).
    static LOTS: RefCell<StableBTreeMap<Vec<u8>, Vec<u8>, Memory>> =
        RefCell::new(StableBTreeMap::init(memory(LOTS_MEMORY)));

    /// Stored candid bytes of EntryRecord, keyed by
    /// lp(chain)‖lp(auction_id)‖lp(lot_id)‖lp(escrow). Entries of one lot are
    /// contiguous; entries of one auction are grouped by lot — the finale
    /// scan (G3) walks this order in bounded portions.
    static ENTRIES: RefCell<StableBTreeMap<Vec<u8>, Vec<u8>, Memory>> =
        RefCell::new(StableBTreeMap::init(memory(ENTRIES_MEMORY)));

    /// lp(chain)‖lp(auction_id)‖lp(escrow) → lot_id: the O(1) way from a
    /// returned escrow address back to its lot.
    static ENTRY_LOT: RefCell<StableBTreeMap<Vec<u8>, Vec<u8>, Memory>> =
        RefCell::new(StableBTreeMap::init(memory(ENTRY_LOT_MEMORY)));

    /// The monotonic registration counter: a total order over entries, the
    /// tie-break of the finale (game-spec §9).
    static SEQ: RefCell<StableCell<u64, Memory>> =
        RefCell::new(StableCell::init(memory(SEQ_MEMORY), 0));

    /// Local-testing override of the SOL RPC canister principal; empty on
    /// real deploys, where the well-known canister id is the only authority.
    static SOL_RPC_OVERRIDE: RefCell<StableCell<Vec<u8>, Memory>> =
        RefCell::new(StableCell::init(memory(SOL_RPC_MEMORY), Vec::new()));

    /// Local-testing override of the crown-index principal; empty on real
    /// deploys, where the baked config value is the only authority.
    static CROWN_INDEX_OVERRIDE: RefCell<StableCell<Vec<u8>, Memory>> =
        RefCell::new(StableCell::init(memory(CROWN_INDEX_MEMORY), Vec::new()));

    /// Local-testing override of the operator wallet; empty on real deploys,
    /// where the baked config value is the only authority.
    static OPERATOR_WALLET_OVERRIDE: RefCell<StableCell<Vec<u8>, Memory>> =
        RefCell::new(StableCell::init(memory(OPERATOR_WALLET_MEMORY), Vec::new()));

    /// (due time, auction key) of every undecided auction; heap index over
    /// stable truth, rebuilt on upgrade. Stale entries are harmless:
    /// processing an auction recomputes its real due time.
    static DUE: RefCell<BTreeSet<(u64, Vec<u8>)>> = const { RefCell::new(BTreeSet::new()) };

    /// Finale scans in flight, keyed by auction key. Heap: an upgrade
    /// forgets them and the scan restarts from the first entry — the
    /// registry is frozen in FINALE_DUE, so a restart folds the same facts.
    static SCANS: RefCell<BTreeMap<Vec<u8>, FinaleScan>> =
        const { RefCell::new(BTreeMap::new()) };
}

pub(crate) fn crown_index() -> Option<candid::Principal> {
    CROWN_INDEX_OVERRIDE.with_borrow(|cell| weight::resolve(cell.get(), CROWN_INDEX))
}

/// The operator wallet: the override if set, else the baked config value.
/// `None` while neither pins one — then no operator return exists.
pub(crate) fn operator_wallet() -> Option<[u8; 32]> {
    let stored = OPERATOR_WALLET_OVERRIDE.with_borrow(|cell| cell.get().clone());
    let bytes = if stored.is_empty() {
        bs58::decode(OPERATOR_WALLET).into_vec().ok()?
    } else {
        stored
    };
    bytes.try_into().ok()
}

pub(crate) fn memory(id: MemoryId) -> Memory {
    MEMORY_MANAGER.with_borrow(|manager| manager.get(id))
}

pub(crate) fn sol_rpc_canister() -> candid::Principal {
    SOL_RPC_OVERRIDE.with_borrow(|cell| {
        let stored = cell.get();
        if stored.is_empty() {
            sol_rpc_client::SOL_RPC_CANISTER
        } else {
            candid::Principal::from_slice(stored)
        }
    })
}

// ---- records ---------------------------------------------------------------

/// Candid mirror of logic::State; conversion at the boundary, like every
/// foreign type (the logic crate knows no candid).
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum StateView {
    #[serde(rename = "bidding")]
    Bidding,
    #[serde(rename = "finale_due")]
    FinaleDue,
    #[serde(rename = "performing")]
    Performing,
    #[serde(rename = "voting")]
    Voting { started_at: u64 },
    #[serde(rename = "done")]
    Done { winner: Option<OutcomeView> },
}

#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeView {
    #[serde(rename = "settle")]
    Settle,
    #[serde(rename = "cancel")]
    Cancel,
}

#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceView {
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "not_done")]
    NotDone,
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VoteView {
    pub voter: serde_bytes::ByteBuf,
    pub choice: ChoiceView,
    pub weight: u128,
}

/// Who returned a lot or an entry, and when — attribution, certified forever.
#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReturnStamp {
    pub at: u64,
    pub by: ActorView,
}

#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorView {
    #[serde(rename = "km")]
    Km,
    #[serde(rename = "operator")]
    Operator,
}

/// The whole stored truth about one auction. `data` of `get_auction` returns
/// the exact candid bytes of this record; the witness hash pins them.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuctionRecord {
    pub chain: String,
    pub auction_id: serde_bytes::ByteBuf,
    pub km: serde_bytes::ByteBuf,
    pub km_nonce: u64,
    pub created_at: u64,
    pub duration: u64,
    pub perform_window: u64,
    /// Snapshot of the profile at birth; an auction carries its own clock
    /// forever.
    pub voting_period: u64,
    /// The KM's floor for one entry's gross; 0 = only the shape's floor.
    pub min_bid: u64,
    pub state: StateView,
    /// Non-empty only from VOTING on; published forever after the verdict.
    pub votes: Vec<VoteView>,
    /// The winning lot, set by the finale (G3); `None` before it and in
    /// auctions that never had a winner.
    pub winner_lot: Option<serde_bytes::ByteBuf>,
    /// Set exactly when the platform operator killed the auction — by
    /// cancelling running bidding or by returning the winner. The
    /// censorship move at auction level, attributed forever.
    pub operator_returned_at: Option<u64>,
}

/// One lot: a text commitment plus the running total of its live entries.
/// The sums are the leaderboard; the truth of the finale is recomputed from
/// the entries (game-spec §9).
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LotRecord {
    pub lot_id: serde_bytes::ByteBuf,
    pub text_hash: serde_bytes::ByteBuf,
    /// The lot's own resolver: the Ed25519 pubkey of the threshold key at
    /// derivation path [lot_id]. Escrows are born with it; membership is
    /// this field (game-spec §2).
    pub resolver: serde_bytes::ByteBuf,
    pub accepted_at: Option<u64>,
    pub returned: Option<ReturnStamp>,
    /// Sum of live (non-returned) registered grosses, minor units.
    pub sum: u128,
    /// Number of registered entries, returned ones included.
    pub entries: u64,
}

/// One registered contribution: the escrow's birth fields as verified
/// against the chain, its registration seq, and its individual return.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EntryRecord {
    pub escrow: serde_bytes::ByteBuf,
    pub lot_id: serde_bytes::ByteBuf,
    pub donor: serde_bytes::ByteBuf,
    pub gross: u64,
    pub deadline: u64,
    pub nonce: u64,
    pub seq: u64,
    pub returned: Option<ReturnStamp>,
}

pub(crate) fn state_to_view(state: &logic::State) -> StateView {
    match state {
        logic::State::Bidding => StateView::Bidding,
        logic::State::FinaleDue => StateView::FinaleDue,
        logic::State::Performing => StateView::Performing,
        logic::State::Voting { started_at } => StateView::Voting {
            started_at: *started_at,
        },
        logic::State::Done { winner } => StateView::Done {
            winner: winner.map(outcome_to_view),
        },
    }
}

pub(crate) fn outcome_to_view(outcome: logic::Outcome) -> OutcomeView {
    match outcome {
        logic::Outcome::Settle => OutcomeView::Settle,
        logic::Outcome::Cancel => OutcomeView::Cancel,
    }
}

fn state_from_view(view: &StateView) -> logic::State {
    match view {
        StateView::Bidding => logic::State::Bidding,
        StateView::FinaleDue => logic::State::FinaleDue,
        StateView::Performing => logic::State::Performing,
        StateView::Voting { started_at } => logic::State::Voting {
            started_at: *started_at,
        },
        StateView::Done { winner } => logic::State::Done {
            winner: winner.map(|outcome| match outcome {
                OutcomeView::Settle => logic::Outcome::Settle,
                OutcomeView::Cancel => logic::Outcome::Cancel,
            }),
        },
    }
}

impl AuctionRecord {
    pub(crate) fn to_logic(&self) -> logic::Auction {
        logic::Auction {
            created_at: self.created_at,
            duration: self.duration,
            perform_window: self.perform_window,
            voting_period: self.voting_period,
            min_bid: self.min_bid,
            state: state_from_view(&self.state),
            votes: self
                .votes
                .iter()
                .map(|vote| logic::Vote {
                    voter: logic::Voter(vote.voter.to_vec()),
                    choice: match vote.choice {
                        ChoiceView::Done => logic::Choice::Done,
                        ChoiceView::NotDone => logic::Choice::NotDone,
                    },
                    weight: vote.weight,
                })
                .collect(),
        }
    }

    pub(crate) fn absorb(&mut self, auction: &logic::Auction) {
        self.state = state_to_view(&auction.state);
        self.votes = auction
            .votes
            .iter()
            .map(|vote| VoteView {
                voter: serde_bytes::ByteBuf::from(vote.voter.0.clone()),
                choice: match vote.choice {
                    logic::Choice::Done => ChoiceView::Done,
                    logic::Choice::NotDone => ChoiceView::NotDone,
                },
                weight: vote.weight,
            })
            .collect();
    }
}

// ---- storage ---------------------------------------------------------------

/// u32-le length-prefixed concatenation: the key material of every map and
/// of the certified tree. Public — a witness verifier must rebuild keys.
fn length_prefixed(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend((part.len() as u32).to_le_bytes());
        out.extend_from_slice(part);
    }
    out
}

pub fn auction_key(chain: &str, auction_id: &[u8]) -> Vec<u8> {
    length_prefixed(&[chain.as_bytes(), auction_id])
}

pub fn lot_key(chain: &str, auction_id: &[u8], lot_id: &[u8]) -> Vec<u8> {
    length_prefixed(&[chain.as_bytes(), auction_id, lot_id])
}

pub fn entry_key(chain: &str, auction_id: &[u8], lot_id: &[u8], escrow: &[u8]) -> Vec<u8> {
    length_prefixed(&[chain.as_bytes(), auction_id, lot_id, escrow])
}

fn entry_lot_key(chain: &str, auction_id: &[u8], escrow: &[u8]) -> Vec<u8> {
    length_prefixed(&[chain.as_bytes(), auction_id, escrow])
}

fn decode<T: CandidType + for<'a> Deserialize<'a>>(bytes: &[u8], what: &str) -> T {
    match Decode!(bytes, T) {
        Ok(record) => record,
        Err(_) => ic_cdk::trap(format!("stable {what}: undecodable record")),
    }
}

fn encode<T: CandidType>(record: &T, what: &str) -> Vec<u8> {
    match Encode!(record) {
        Ok(bytes) => bytes,
        Err(_) => ic_cdk::trap(format!("{what}: encode failed")),
    }
}

pub(crate) fn load_auction_bytes(key: &[u8]) -> Option<Vec<u8>> {
    AUCTIONS.with_borrow(|auctions| auctions.get(&key.to_vec()))
}

pub(crate) fn load_auction(key: &[u8]) -> Option<AuctionRecord> {
    load_auction_bytes(key).map(|bytes| decode(&bytes, "auctions"))
}

/// Persists an auction record, refreshes the certified tree and the due
/// index. The single write path: every auction mutation ends here.
pub(crate) fn save_auction(key: &[u8], record: &AuctionRecord) {
    let bytes = encode(record, "auction record");
    AUCTIONS.with_borrow_mut(|auctions| auctions.insert(key.to_vec(), bytes.clone()));
    certify::upsert(key, &bytes);
    if let Some(due) = due_of(record) {
        DUE.with_borrow_mut(|set| set.insert((due, key.to_vec())));
    }
}

fn due_of(record: &AuctionRecord) -> Option<u64> {
    match record.state {
        StateView::Bidding => Some(record.created_at.saturating_add(record.duration)),
        // The scan is due the moment the state exists; chunks re-arm a
        // near-immediate tick themselves.
        StateView::FinaleDue => Some(0),
        StateView::Performing => Some(
            record
                .created_at
                .saturating_add(record.duration)
                .saturating_add(record.perform_window),
        ),
        StateView::Voting { started_at } => Some(started_at.saturating_add(record.voting_period)),
        StateView::Done { .. } => None,
    }
}

pub(crate) fn load_lot_bytes(key: &[u8]) -> Option<Vec<u8>> {
    LOTS.with_borrow(|lots| lots.get(&key.to_vec()))
}

pub(crate) fn load_lot(key: &[u8]) -> Option<LotRecord> {
    load_lot_bytes(key).map(|bytes| decode(&bytes, "lots"))
}

pub(crate) fn save_lot(key: &[u8], record: &LotRecord) {
    let bytes = encode(record, "lot record");
    LOTS.with_borrow_mut(|lots| lots.insert(key.to_vec(), bytes.clone()));
    certify::upsert(key, &bytes);
}

pub(crate) fn load_entry(key: &[u8]) -> Option<EntryRecord> {
    ENTRIES
        .with_borrow(|entries| entries.get(&key.to_vec()))
        .map(|bytes| decode(&bytes, "entries"))
}

pub(crate) fn save_entry(key: &[u8], record: &EntryRecord) {
    let bytes = encode(record, "entry record");
    ENTRIES.with_borrow_mut(|entries| entries.insert(key.to_vec(), bytes.clone()));
    certify::upsert(key, &bytes);
}

/// The lot an escrow registered under, if any.
pub(crate) fn lot_of_entry(chain: &str, auction_id: &[u8], escrow: &[u8]) -> Option<Vec<u8>> {
    ENTRY_LOT.with_borrow(|index| index.get(&entry_lot_key(chain, auction_id, escrow)))
}

pub(crate) fn index_entry(chain: &str, auction_id: &[u8], escrow: &[u8], lot_id: &[u8]) {
    ENTRY_LOT.with_borrow_mut(|index| {
        index.insert(entry_lot_key(chain, auction_id, escrow), lot_id.to_vec())
    });
}

/// The next value of the monotonic registration counter.
pub(crate) fn next_seq() -> u64 {
    SEQ.with_borrow_mut(|cell| {
        let next = cell.get().saturating_add(1);
        cell.set(next);
        next
    })
}

pub(crate) fn lots_of_auction(chain: &str, auction_id: &[u8]) -> Vec<LotRecord> {
    let prefix = auction_key(chain, auction_id);
    LOTS.with_borrow(|lots| {
        lots.range(prefix.clone()..)
            .take_while(|entry| entry.key().starts_with(&prefix))
            .map(|entry| decode(&entry.value(), "lots"))
            .collect()
    })
}

pub(crate) fn entries_of_lot(chain: &str, auction_id: &[u8], lot_id: &[u8]) -> Vec<EntryRecord> {
    let prefix = lot_key(chain, auction_id, lot_id);
    ENTRIES.with_borrow(|entries| {
        entries
            .range(prefix.clone()..)
            .take_while(|entry| entry.key().starts_with(&prefix))
            .map(|entry| decode(&entry.value(), "entries"))
            .collect()
    })
}

// ---- time and the finale scan ----------------------------------------------

pub(crate) fn now_seconds() -> u64 {
    ic_cdk::api::time() / 1_000_000_000
}

/// The timer only backstops "time first" inside every step: a late tick can
/// delay a due transition, never corrupt it.
const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// How many due auctions one timer tick processes before yielding — the
/// bound that keeps a burst of simultaneous deadlines from trapping the
/// tick (the Сбор lesson).
const MAX_DUE_PER_TICK: usize = 50;

/// How many entries one finale scan slice folds before yielding.
const SCAN_CHUNK: usize = 50;

/// A finale scan in flight: the resume cursor into the entries map, the lot
/// being collected (entries of one lot are contiguous under the key order),
/// and the running best.
#[derive(Default)]
struct FinaleScan {
    cursor: Option<Vec<u8>>,
    current: Option<(Vec<u8>, Vec<logic::Entry>)>,
    best: Option<(Vec<u8>, logic::Standing)>,
}

/// Folds a fully-collected lot into the running best. Only accepted,
/// unreturned lots with a live standing race (game-spec §9); the comparison
/// is `logic::beats` — the same law `logic::winner` folds with.
fn fold_scanned_lot(chain: &str, auction_id: &[u8], scan: &mut FinaleScan) {
    let Some((lot_id, entries)) = scan.current.take() else {
        return;
    };
    let Some(lot) = load_lot(&lot_key(chain, auction_id, &lot_id)) else {
        return;
    };
    if lot.accepted_at.is_none() || lot.returned.is_some() {
        return;
    }
    let Some(standing) = logic::standing(&entries) else {
        return;
    };
    let replace = match &scan.best {
        None => true,
        Some((_, incumbent)) => logic::beats(&standing, incumbent),
    };
    if replace {
        scan.best = Some((lot_id, standing));
    }
}

/// One bounded slice of the finale scan (game-spec §9). The registry is
/// frozen in FINALE_DUE — no registrations, accepts or returns — so slices
/// across ticks measure one immutable composition. Returns true when the
/// finale was applied and the record saved.
fn finale_scan_chunk(akey: &[u8], record: &mut AuctionRecord, now: u64) -> bool {
    let mut scan = SCANS
        .with_borrow_mut(|scans| scans.remove(&akey.to_vec()))
        .unwrap_or_default();
    let auction_id = record.auction_id.to_vec();
    let chain = record.chain.clone();
    let prefix = auction_key(&chain, &auction_id);

    let start = match &scan.cursor {
        Some(cursor) => std::ops::Bound::Excluded(cursor.clone()),
        None => std::ops::Bound::Included(prefix.clone()),
    };
    let chunk: Vec<(Vec<u8>, EntryRecord)> = ENTRIES.with_borrow(|entries| {
        entries
            .range((start, std::ops::Bound::Unbounded))
            .take_while(|e| e.key().starts_with(&prefix))
            .take(SCAN_CHUNK)
            .map(|e| (e.key().clone(), decode(&e.value(), "entries")))
            .collect()
    });
    let finished = chunk.len() < SCAN_CHUNK;
    for (key, entry) in chunk {
        let lot_id = entry.lot_id.to_vec();
        if scan.current.as_ref().map(|(id, _)| id) != Some(&lot_id) {
            fold_scanned_lot(&chain, &auction_id, &mut scan);
            scan.current = Some((lot_id, Vec::new()));
        }
        if let Some((_, collected)) = &mut scan.current {
            collected.push(logic::Entry {
                gross: entry.gross,
                seq: entry.seq,
                alive: entry.returned.is_none(),
            });
        }
        scan.cursor = Some(key);
    }
    if !finished {
        SCANS.with_borrow_mut(|scans| scans.insert(akey.to_vec(), scan));
        return false;
    }
    fold_scanned_lot(&chain, &auction_id, &mut scan);
    let winner = scan.best.take();
    let mut auction = record.to_logic();
    if logic::step(
        &mut auction,
        logic::Action::Finale {
            winner: winner.is_some(),
        },
        now,
    )
    .is_err()
    {
        // Unreachable from FINALE_DUE; refusing to guess leaves the record
        // untouched for the next tick.
        return true;
    }
    record.winner_lot = winner.map(|(lot_id, _)| serde_bytes::ByteBuf::from(lot_id));
    record.absorb(&auction);
    save_auction(akey, record);
    true
}

/// Applies due time transitions to at most `MAX_DUE_PER_TICK` auctions.
/// Saving re-inserts the auction's next due time. Returns `true` when due
/// work remains — the timer then re-arms a near-immediate tick.
fn process_due(now: u64) -> bool {
    for _ in 0..MAX_DUE_PER_TICK {
        let entry = DUE.with_borrow(|set| set.first().cloned());
        let Some((due, key)) = entry else {
            return false;
        };
        if due > now {
            return false;
        }
        DUE.with_borrow_mut(|set| set.remove(&(due, key.clone())));
        let Some(mut record) = load_auction(&key) else {
            continue;
        };
        if record.state == StateView::FinaleDue {
            // One bounded scan slice; unfinished work re-arms instead of
            // spinning this loop with the same auction.
            if !finale_scan_chunk(&key, &mut record, now) {
                DUE.with_borrow_mut(|set| set.insert((0, key)));
                return true;
            }
            continue;
        }
        let before = record.clone();
        let mut auction = record.to_logic();
        // On success the state advances and is re-saved; an unchanged tick
        // (a stale due entry) writes nothing and re-inserts nothing.
        if logic::step(&mut auction, logic::Action::Tick, now).is_ok() {
            record.absorb(&auction);
            if record != before {
                save_auction(&key, &record);
            }
        }
    }
    DUE.with_borrow(|set| set.first().is_some_and(|(due, _)| *due <= now))
}

fn schedule_tick(delay: Duration) {
    let now = ic_cdk::api::time();
    ic_cdk::api::global_timer_set(now.saturating_add(delay.as_nanos() as u64));
}

#[cfg_attr(target_family = "wasm", unsafe(export_name = "canister_global_timer"))]
#[allow(dead_code)]
fn global_timer() {
    // Re-arm first: a trap inside processing must not stop the schedule.
    schedule_tick(TICK_INTERVAL);
    if process_due(now_seconds()) {
        schedule_tick(Duration::from_secs(1));
    }
}

// ---- lifecycle ---------------------------------------------------------------

/// Local-testing overrides, for replicas where the real SOL RPC canister,
/// crown-index or operator key do not exist. Forbidden on mainnet: there
/// the baked config and the well-known canister ids are the only truth.
#[derive(CandidType, Deserialize)]
pub struct Overrides {
    pub sol_rpc: Option<candid::Principal>,
    pub crown_index: Option<candid::Principal>,
    pub operator_wallet: Option<serde_bytes::ByteBuf>,
}

#[ic_cdk::init]
fn init(overrides: Option<Overrides>) {
    if let Err(error) = auth::validate_config() {
        ic_cdk::trap(error.text());
    }
    if let Some(overrides) = overrides {
        if PROFILE == "mainnet" {
            ic_cdk::trap("mainnet profile: overrides are forbidden");
        }
        if let Some(principal) = overrides.sol_rpc {
            SOL_RPC_OVERRIDE.with_borrow_mut(|cell| cell.set(principal.as_slice().to_vec()));
        }
        if let Some(principal) = overrides.crown_index {
            CROWN_INDEX_OVERRIDE.with_borrow_mut(|cell| cell.set(principal.as_slice().to_vec()));
        }
        if let Some(wallet) = overrides.operator_wallet {
            if wallet.len() != 32 {
                ic_cdk::trap("operator wallet override: not 32 bytes");
            }
            OPERATOR_WALLET_OVERRIDE.with_borrow_mut(|cell| cell.set(wallet.into_vec()));
        }
    }
    certify::recertify();
    schedule_tick(Duration::from_secs(1));
}

#[ic_cdk::post_upgrade]
fn post_upgrade() {
    if let Err(error) = auth::validate_config() {
        ic_cdk::trap(error.text());
    }
    let mut all: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    AUCTIONS.with_borrow(|map| {
        for entry in map.iter() {
            let record: AuctionRecord = decode(&entry.value(), "auctions");
            if let Some(due) = due_of(&record) {
                DUE.with_borrow_mut(|set| {
                    set.insert((due, entry.key().clone()));
                });
            }
            all.push((entry.key().clone(), entry.value().clone()));
        }
    });
    LOTS.with_borrow(|map| {
        all.extend(map.iter().map(|e| (e.key().clone(), e.value().clone())));
    });
    ENTRIES.with_borrow(|map| {
        all.extend(map.iter().map(|e| (e.key().clone(), e.value().clone())));
    });
    certify::rebuild(all.into_iter());
    schedule_tick(Duration::from_secs(1));
}
