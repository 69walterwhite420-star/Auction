//! G3 — финал, голосование, подписи (docs/build-plan.md). PocketIC: таймеры
//! доводят торги до финала порционным сканом, голоса взвешиваются мок-книгой,
//! вердикты подписываются threshold-ключом лота и проверяются оффчейн против
//! его резолвера — это и есть паритет пути деривации.

mod common;

use auction::{ActorView, ChoiceView, OutcomeView, StateView};
use common::*;

const TEXT_A: [u8; 32] = [0xA1; 32];
const TEXT_B: [u8; 32] = [0xB2; 32];
const TEXT_C: [u8; 32] = [0xC3; 32];

fn created_at(s: &Setup, auction_id: &[u8]) -> u64 {
    auction_state(&fetch_auction(s, auction_id)).created_at
}

fn state_of(s: &Setup, auction_id: &[u8]) -> StateView {
    auction_state(&fetch_auction(s, auction_id)).state
}

fn winner_lot(s: &Setup, auction_id: &[u8]) -> Option<Vec<u8>> {
    auction_state(&fetch_auction(s, auction_id))
        .winner_lot
        .map(|w| w.to_vec())
}

// ---- the finale ----------------------------------------------------------------

#[test]
#[ignore]
fn finale_picks_the_richest_accepted_lot() {
    let s = setup();
    let km = wallet(0x10);
    let auction_id = create_auction(&s, &km, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));

    // A: 1.5M over two entries, accepted. B: 2M, accepted. C: 3M, never
    // accepted — the richest money loses to the richest *accepted* money.
    let a1 = plant_bid(&s, &auction_id, &km, 0x21, TEXT_A, 1_000_000, deadline, 1);
    register_bid(&s, &auction_id, &a1).expect("register");
    let a2 = plant_bid(&s, &auction_id, &km, 0x22, TEXT_A, 500_000, deadline, 2);
    register_bid(&s, &auction_id, &a2).expect("register");
    let b = plant_bid(&s, &auction_id, &km, 0x23, TEXT_B, 2_000_000, deadline, 3);
    register_bid(&s, &auction_id, &b).expect("register");
    let c = plant_bid(&s, &auction_id, &km, 0x24, TEXT_C, 3_000_000, deadline, 4);
    register_bid(&s, &auction_id, &c).expect("register");
    accept_lot(&s, &auction_id, a1.lot_id, &km).expect("accept A");
    accept_lot(&s, &auction_id, b.lot_id, &km).expect("accept B");

    advance(&s, DURATION);
    assert_eq!(state_of(&s, &auction_id), StateView::Performing);
    assert_eq!(winner_lot(&s, &auction_id), Some(b.lot_id.to_vec()));

    // Losers are freed the moment the finale stands; the winner waits.
    let verdict = request_signature(&s, &auction_id, &a1).expect("loser signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);
    assert_eq!(verdict.escrow.to_vec(), a1.escrow);
    verify_verdict(&a1.resolver, &a1.escrow, 1, &verdict.signature);
    let verdict = request_signature(&s, &auction_id, &c).expect("unaccepted signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);
    verify_verdict(&c.resolver, &c.escrow, 1, &verdict.signature);
    assert_eq!(
        request_signature(&s, &auction_id, &b).unwrap_err(),
        "no verdict yet"
    );
}

#[test]
#[ignore]
fn finale_tie_goes_to_the_earliest_composition() {
    let s = setup();
    let km = wallet(0x10);

    // Auction 1: A reaches 300k first (seq 1); B assembles the same sum
    // later (seqs 2, 3). A wins the tie.
    let auction_a = create_auction(&s, &km, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_a));
    let a = plant_bid(&s, &auction_a, &km, 0x21, TEXT_A, 300_000, deadline, 1);
    register_bid(&s, &auction_a, &a).expect("register");
    let b1 = plant_bid(&s, &auction_a, &km, 0x22, TEXT_B, 100_000, deadline, 2);
    register_bid(&s, &auction_a, &b1).expect("register");
    let b2 = plant_bid(&s, &auction_a, &km, 0x23, TEXT_B, 200_000, deadline, 3);
    register_bid(&s, &auction_a, &b2).expect("register");
    accept_lot(&s, &auction_a, a.lot_id, &km).expect("accept");
    accept_lot(&s, &auction_a, b1.lot_id, &km).expect("accept");
    advance(&s, DURATION);
    assert_eq!(winner_lot(&s, &auction_a), Some(a.lot_id.to_vec()));

    // Auction 2: the same composition, but A's entry is returned before the
    // finale — B stands alone and wins.
    let auction_b = create_auction(&s, &km, 2).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_b));
    let a = plant_bid(&s, &auction_b, &km, 0x31, TEXT_A, 300_000, deadline, 1);
    register_bid(&s, &auction_b, &a).expect("register");
    let b = plant_bid(&s, &auction_b, &km, 0x32, TEXT_B, 300_000, deadline, 2);
    register_bid(&s, &auction_b, &b).expect("register");
    accept_lot(&s, &auction_b, a.lot_id, &km).expect("accept");
    accept_lot(&s, &auction_b, b.lot_id, &km).expect("accept");
    return_entry(&s, &auction_b, &a.escrow, &km).expect("return");
    advance(&s, DURATION);
    assert_eq!(winner_lot(&s, &auction_b), Some(b.lot_id.to_vec()));
}

#[test]
#[ignore]
fn finale_without_candidates_dies_unwon() {
    let s = setup();
    let km = wallet(0x10);

    // (a) No lots at all.
    let empty = create_auction(&s, &km, 1).expect("create");
    // (b) Registered but never accepted.
    let unaccepted = create_auction(&s, &km, 2).expect("create");
    let deadline = good_deadline(created_at(&s, &unaccepted));
    let bid = plant_bid(&s, &unaccepted, &km, 0x21, TEXT_A, 1_000_000, deadline, 1);
    register_bid(&s, &unaccepted, &bid).expect("register");
    // (c) Accepted, then the whole lot returned.
    let returned = create_auction(&s, &km, 3).expect("create");
    let deadline = good_deadline(created_at(&s, &returned));
    let bid_c = plant_bid(&s, &returned, &km, 0x22, TEXT_A, 1_000_000, deadline, 1);
    register_bid(&s, &returned, &bid_c).expect("register");
    accept_lot(&s, &returned, bid_c.lot_id, &km).expect("accept");
    return_lot(&s, &returned, bid_c.lot_id, &km).expect("return");
    // (d) Accepted, but every entry individually returned: zero stands.
    let drained = create_auction(&s, &km, 4).expect("create");
    let deadline = good_deadline(created_at(&s, &drained));
    let bid_d = plant_bid(&s, &drained, &km, 0x23, TEXT_A, 1_000_000, deadline, 1);
    register_bid(&s, &drained, &bid_d).expect("register");
    accept_lot(&s, &drained, bid_d.lot_id, &km).expect("accept");
    return_entry(&s, &drained, &bid_d.escrow, &km).expect("return");

    advance(&s, DURATION);
    for auction_id in [&empty, &unaccepted, &returned, &drained] {
        assert_eq!(
            state_of(&s, auction_id),
            StateView::Done { winner: None },
            "auction died unwon"
        );
    }

    // Every deriving escrow of a dead auction resolves to cancel.
    let verdict = request_signature(&s, &unaccepted, &bid).expect("cancel signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);
    verify_verdict(&bid.resolver, &bid.escrow, 1, &verdict.signature);
}

#[test]
#[ignore]
fn finale_survives_a_burst_of_lots() {
    // More entries than one scan slice folds: the finale must span ticks
    // and still pick the exact maximum.
    let s = setup();
    let km = wallet(0x10);
    let auction_id = create_auction(&s, &km, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));

    let mut best_lot = None;
    for i in 0..55u64 {
        let mut text = [0u8; 32];
        text[..8].copy_from_slice(&i.to_le_bytes());
        text[31] = 0x77;
        let gross = 100_000 + i * 1_000;
        let bid = plant_bid(
            &s,
            &auction_id,
            &km,
            (0x20 + (i % 200)) as u8,
            text,
            gross,
            deadline,
            i,
        );
        register_bid(&s, &auction_id, &bid).expect("register");
        accept_lot(&s, &auction_id, bid.lot_id, &km).expect("accept");
        if i == 54 {
            best_lot = Some(bid.lot_id.to_vec());
        }
    }

    advance(&s, DURATION);
    assert_eq!(state_of(&s, &auction_id), StateView::Performing);
    assert_eq!(winner_lot(&s, &auction_id), best_lot);
}

// ---- ready and voting ----------------------------------------------------------

/// A winner-in-performing auction: two lots, B the richer, both accepted.
fn performing(s: &Setup, km: &Wallet, km_nonce: u64) -> (Vec<u8>, Bid, Bid) {
    let auction_id = create_auction(s, km, km_nonce).expect("create");
    let deadline = good_deadline(created_at(s, &auction_id));
    let loser = plant_bid(s, &auction_id, km, 0x21, TEXT_A, 1_000_000, deadline, 1);
    register_bid(s, &auction_id, &loser).expect("register");
    let winner = plant_bid(s, &auction_id, km, 0x22, TEXT_B, 2_000_000, deadline, 2);
    register_bid(s, &auction_id, &winner).expect("register");
    accept_lot(s, &auction_id, loser.lot_id, km).expect("accept");
    accept_lot(s, &auction_id, winner.lot_id, km).expect("accept");
    advance(s, DURATION);
    assert_eq!(state_of(s, &auction_id), StateView::Performing);
    (auction_id, winner, loser)
}

#[test]
#[ignore]
fn ready_opens_voting_and_votes_settle() {
    let s = setup();
    let km = wallet(0x10);
    let (auction_id, winner, loser) = performing(&s, &km, 1);

    let stranger = wallet(0x66);
    assert_eq!(
        ready(&s, &auction_id, &stranger).unwrap_err(),
        "bad signature"
    );
    ready(&s, &auction_id, &km).expect("ready");
    assert!(matches!(
        state_of(&s, &auction_id),
        StateView::Voting { .. }
    ));
    assert_eq!(
        ready(&s, &auction_id, &km).unwrap_err(),
        "invalid transition"
    );

    // Votes: weight comes from the book; the threshold and the dedup hold.
    let yes = wallet(0x41);
    let no = wallet(0x42);
    let featherweight = wallet(0x43);
    seed_reputation(&s, &yes.address, &km.address, 500_000);
    seed_reputation(&s, &no.address, &km.address, 200_000);
    seed_reputation(&s, &featherweight.address, &km.address, 99_999);
    vote(&s, &auction_id, &yes, ChoiceView::Done).expect("vote done");
    vote(&s, &auction_id, &no, ChoiceView::NotDone).expect("vote not_done");
    assert_eq!(
        vote(&s, &auction_id, &featherweight, ChoiceView::Done).unwrap_err(),
        "vote weight below threshold"
    );
    assert_eq!(
        vote(&s, &auction_id, &yes, ChoiceView::NotDone).unwrap_err(),
        "duplicate voter"
    );

    advance(&s, VOTING_PERIOD);
    assert_eq!(
        state_of(&s, &auction_id),
        StateView::Done {
            winner: Some(OutcomeView::Settle)
        }
    );
    let record = auction_state(&fetch_auction(&s, &auction_id));
    assert_eq!(record.votes.len(), 2, "the vote roll is published forever");

    // The winner settles; the loser stays cancelled; both verify offchain.
    let verdict = request_signature(&s, &auction_id, &winner).expect("settle signs");
    assert_eq!(verdict.outcome, OutcomeView::Settle);
    verify_verdict(&winner.resolver, &winner.escrow, 0, &verdict.signature);
    let verdict = request_signature(&s, &auction_id, &loser).expect("loser signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);

    // A retry re-signs the same resolution — another valid signature, no
    // second outcome.
    let retry = request_signature(&s, &auction_id, &winner).expect("retry");
    assert_eq!(retry.outcome, OutcomeView::Settle);
    verify_verdict(&winner.resolver, &winner.escrow, 0, &retry.signature);

    // The verdict is untouchable: no return moves it.
    assert_eq!(
        return_lot(&s, &auction_id, winner.lot_id, &km).unwrap_err(),
        "invalid transition"
    );
    assert_eq!(
        operator_refund_lot(&s, &auction_id, winner.lot_id, &operator()).unwrap_err(),
        "invalid transition"
    );
}

#[test]
#[ignore]
fn silence_and_dissent_cancel() {
    let s = setup();
    let km = wallet(0x10);

    // Silence: nobody voted.
    let (auction_a, winner_a, _) = performing(&s, &km, 1);
    ready(&s, &auction_a, &km).expect("ready");
    advance(&s, VOTING_PERIOD);
    assert_eq!(
        state_of(&s, &auction_a),
        StateView::Done {
            winner: Some(OutcomeView::Cancel)
        }
    );
    let verdict = request_signature(&s, &auction_a, &winner_a).expect("cancel signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);
    verify_verdict(&winner_a.resolver, &winner_a.escrow, 1, &verdict.signature);

    // Dissent: the weighted "not done" outweighs.
    let (auction_b, _, _) = performing(&s, &km, 2);
    ready(&s, &auction_b, &km).expect("ready");
    let yes = wallet(0x41);
    let no = wallet(0x42);
    seed_reputation(&s, &yes.address, &km.address, 200_000);
    seed_reputation(&s, &no.address, &km.address, 200_001);
    vote(&s, &auction_b, &yes, ChoiceView::Done).expect("vote");
    vote(&s, &auction_b, &no, ChoiceView::NotDone).expect("vote");
    advance(&s, VOTING_PERIOD);
    assert_eq!(
        state_of(&s, &auction_b),
        StateView::Done {
            winner: Some(OutcomeView::Cancel)
        }
    );

    // Late vote: the tally happened first.
    let late = wallet(0x44);
    seed_reputation(&s, &late.address, &km.address, 900_000);
    assert_eq!(
        vote(&s, &auction_b, &late, ChoiceView::Done).unwrap_err(),
        "invalid transition"
    );
}

#[test]
#[ignore]
fn perform_expiry_cancels_the_winner() {
    let s = setup();
    let km = wallet(0x10);
    let (auction_id, winner, _) = performing(&s, &km, 1);

    advance(&s, PERFORM_WINDOW);
    assert_eq!(
        state_of(&s, &auction_id),
        StateView::Done {
            winner: Some(OutcomeView::Cancel)
        }
    );
    assert_eq!(
        ready(&s, &auction_id, &km).unwrap_err(),
        "invalid transition"
    );
    let verdict = request_signature(&s, &auction_id, &winner).expect("cancel signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);
}

#[test]
#[ignore]
fn km_returns_the_winner_before_ready_only() {
    let s = setup();
    let km = wallet(0x10);

    // Before "ready": the auction dies unsettled, the lot is stamped.
    let (auction_a, winner_a, _) = performing(&s, &km, 1);
    return_lot(&s, &auction_a, winner_a.lot_id, &km).expect("return winner");
    assert_eq!(
        state_of(&s, &auction_a),
        StateView::Done {
            winner: Some(OutcomeView::Cancel)
        }
    );
    let lot = lot_state(&fetch_lot(&s, &auction_a, &winner_a.lot_id));
    assert_eq!(lot.returned.expect("stamped").by, ActorView::Km);
    let verdict = request_signature(&s, &auction_a, &winner_a).expect("cancel signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);

    // After "ready" the KM's door is closed.
    let (auction_b, winner_b, _) = performing(&s, &km, 2);
    ready(&s, &auction_b, &km).expect("ready");
    assert_eq!(
        return_lot(&s, &auction_b, winner_b.lot_id, &km).unwrap_err(),
        "invalid transition"
    );
}

// ---- the operator ---------------------------------------------------------------

#[test]
#[ignore]
fn operator_returns_lots_entries_and_the_winner() {
    let s = setup();
    let km = wallet(0x10);
    let stranger = wallet(0x66);

    // In BIDDING: any lot, stamped with the operator's attribution.
    let auction_a = create_auction(&s, &km, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_a));
    let bid = plant_bid(&s, &auction_a, &km, 0x21, TEXT_A, 1_000_000, deadline, 1);
    register_bid(&s, &auction_a, &bid).expect("register");
    assert_eq!(
        operator_refund_lot(&s, &auction_a, bid.lot_id, &stranger).unwrap_err(),
        "bad signature"
    );
    operator_refund_lot(&s, &auction_a, bid.lot_id, &operator()).expect("operator returns");
    let lot = lot_state(&fetch_lot(&s, &auction_a, &bid.lot_id));
    assert_eq!(lot.returned.expect("stamped").by, ActorView::Operator);

    // The winner during VOTING: the auction dies, votes stay published,
    // the move is attributed at auction level.
    let (auction_b, winner_b, _) = performing(&s, &km, 2);
    ready(&s, &auction_b, &km).expect("ready");
    let yes = wallet(0x41);
    seed_reputation(&s, &yes.address, &km.address, 500_000);
    vote(&s, &auction_b, &yes, ChoiceView::Done).expect("vote");
    operator_refund_lot(&s, &auction_b, winner_b.lot_id, &operator()).expect("operator");
    let record = auction_state(&fetch_auction(&s, &auction_b));
    assert_eq!(
        record.state,
        StateView::Done {
            winner: Some(OutcomeView::Cancel)
        }
    );
    assert!(record.operator_returned_at.is_some());
    assert_eq!(record.votes.len(), 1);

    // One entry of the winner during PERFORMING: the entry gets cancel and
    // keeps it even when the lot later settles. The top-up registered while
    // bidding still ran — the registry closes at the finale.
    let auction_c = create_auction(&s, &km, 3).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_c));
    let winner_c = plant_bid(&s, &auction_c, &km, 0x33, TEXT_B, 2_000_000, deadline, 1);
    register_bid(&s, &auction_c, &winner_c).expect("register");
    let topup = plant_bid(&s, &auction_c, &km, 0x35, TEXT_B, 300_000, deadline, 2);
    register_bid(&s, &auction_c, &topup).expect("register");
    accept_lot(&s, &auction_c, winner_c.lot_id, &km).expect("accept");
    advance(&s, DURATION);
    assert_eq!(state_of(&s, &auction_c), StateView::Performing);
    operator_refund_entry(&s, &auction_c, &topup.escrow, &operator()).expect("operator entry");
    ready(&s, &auction_c, &km).expect("ready");
    vote(&s, &auction_c, &yes, ChoiceView::Done).expect("vote");
    advance(&s, VOTING_PERIOD);
    assert_eq!(
        state_of(&s, &auction_c),
        StateView::Done {
            winner: Some(OutcomeView::Settle)
        }
    );
    let settled = request_signature(&s, &auction_c, &winner_c).expect("settle");
    assert_eq!(settled.outcome, OutcomeView::Settle);
    verify_verdict(&winner_c.resolver, &winner_c.escrow, 0, &settled.signature);
    let overridden = request_signature(&s, &auction_c, &topup).expect("entry override");
    assert_eq!(overridden.outcome, OutcomeView::Cancel);
    verify_verdict(&topup.resolver, &topup.escrow, 1, &overridden.signature);
}

// ---- signature-on-demand edges --------------------------------------------------

#[test]
#[ignore]
fn signatures_wait_for_verdicts_and_cover_unknown_lots() {
    let s = setup();
    let km = wallet(0x10);
    let auction_id = create_auction(&s, &km, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let bid = plant_bid(&s, &auction_id, &km, 0x21, TEXT_A, 1_000_000, deadline, 1);
    register_bid(&s, &auction_id, &bid).expect("register");
    accept_lot(&s, &auction_id, bid.lot_id, &km).expect("accept");

    // No verdict while bidding runs; unknown auctions error.
    assert_eq!(
        request_signature(&s, &auction_id, &bid).unwrap_err(),
        "no verdict yet"
    );
    assert_eq!(
        request_signature_raw(&s, &[0x0D; 32], &TEXT_A, &bid.donor.address, 1, deadline, 1)
            .unwrap_err(),
        "unknown auction"
    );

    // The KM cancels the whole auction: even a lot the registry never heard
    // of resolves to cancel — unregistered means unaccepted means lost.
    cancel_auction(&s, &auction_id, &km).expect("cancel");
    let ghost_donor = wallet(0x77);
    let ghost = request_signature_raw(
        &s,
        &auction_id,
        &TEXT_C,
        &ghost_donor.address,
        500_000,
        deadline,
        9,
    )
    .expect("ghost lot cancel signs");
    assert_eq!(ghost.outcome, OutcomeView::Cancel);
    let ghost_resolver = get_resolver(&s, &auction_id, &TEXT_C).expect("resolver");
    verify_verdict(&ghost_resolver, &ghost.escrow, 1, &ghost.signature);

    // A verdict signature binds its lot: it must not verify against a
    // foreign lot's resolver.
    let foreign = get_resolver(&s, &auction_id, &TEXT_B).expect("resolver");
    let key: [u8; 32] = foreign.as_slice().try_into().unwrap();
    let signature: [u8; 64] = ghost.signature.as_slice().try_into().unwrap();
    let mut message = Vec::new();
    message.extend_from_slice(b"crown:two-outcome:solana-devnet");
    message.extend_from_slice(&bs58::decode(FACTORY).into_vec().unwrap());
    message.extend_from_slice(&ghost.escrow);
    message.push(1);
    assert!(
        ed25519_dalek::VerifyingKey::from_bytes(&key)
            .unwrap()
            .verify_strict(&message, &ed25519_dalek::Signature::from_bytes(&signature))
            .is_err(),
        "a lot's verdict must not verify under another lot's resolver"
    );
}

#[test]
#[ignore]
fn operator_cancels_running_bidding_altogether() {
    let s = setup();
    let km = wallet(0x10);
    let stranger = wallet(0x66);

    let auction_id = create_auction(&s, &km, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let bid = plant_bid(&s, &auction_id, &km, 0x21, TEXT_A, 1_000_000, deadline, 1);
    register_bid(&s, &auction_id, &bid).expect("register");
    accept_lot(&s, &auction_id, bid.lot_id, &km).expect("accept");

    assert_eq!(
        operator_cancel_auction(&s, &auction_id, &stranger).unwrap_err(),
        "bad signature"
    );
    operator_cancel_auction(&s, &auction_id, &operator()).expect("operator cancels");
    let record = auction_state(&fetch_auction(&s, &auction_id));
    assert_eq!(record.state, StateView::Done { winner: None });
    assert!(record.operator_returned_at.is_some(), "attributed forever");

    // The dead auction takes nothing and every lot resolves to cancel —
    // the registered one and one the registry never heard of alike.
    let late = plant_bid(&s, &auction_id, &km, 0x22, TEXT_B, 500_000, deadline, 2);
    assert_eq!(
        register_bid(&s, &auction_id, &late).unwrap_err(),
        "invalid transition"
    );
    let verdict = request_signature(&s, &auction_id, &bid).expect("cancel signs");
    assert_eq!(verdict.outcome, OutcomeView::Cancel);
    verify_verdict(&bid.resolver, &bid.escrow, 1, &verdict.signature);
    let ghost = request_signature(&s, &auction_id, &late).expect("ghost lot cancel signs");
    assert_eq!(ghost.outcome, OutcomeView::Cancel);
    assert_eq!(
        operator_cancel_auction(&s, &auction_id, &operator()).unwrap_err(),
        "invalid transition"
    );

    // After the finale the whole-auction door is closed: the operator kills
    // an auction by returning its winner instead.
    let (auction_b, _, _) = performing(&s, &km, 2);
    assert_eq!(
        operator_cancel_auction(&s, &auction_b, &operator()).unwrap_err(),
        "invalid transition"
    );
}
