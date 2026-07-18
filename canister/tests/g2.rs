//! G2 — bidding: the registry, the chain read, returns (docs/build-plan.md).
//! PocketIC: the game canister next to the SOL RPC mock; escrow fixtures are
//! planted into the mock and registration must tell real from fake.

mod common;

use auction::{StateView, auth};
use candid::Encode;
use common::*;

const TEXT_A: [u8; 32] = [0xA1; 32];
const TEXT_B: [u8; 32] = [0xB2; 32];

fn created_at(s: &Setup, auction_id: &[u8]) -> u64 {
    auction_state(&fetch_auction(s, auction_id)).created_at
}

// ---- creation ----------------------------------------------------------------

#[test]
#[ignore]
fn create_births_a_certified_bidding_auction() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");

    let certified = fetch_auction(&s, &auction_id);
    let record = auction_state(&certified);
    assert_eq!(record.chain, CHAIN);
    assert_eq!(record.recipient.as_slice(), recipient.address.as_slice());
    assert_eq!(record.recipient_nonce, 1);
    assert_eq!(record.duration, DURATION);
    assert_eq!(record.perform_window, PERFORM_WINDOW);
    assert_eq!(record.voting_period, VOTING_PERIOD);
    assert_eq!(record.min_entry, MIN_ENTRY);
    assert_eq!(record.state, StateView::Bidding);
    assert!(record.votes.is_empty());
    assert!(record.winner_lot.is_none());
    assert!(record.operator_returned_at.is_none());
    verify_certified_auction(&s, &auction_id, &certified);
}

#[test]
#[ignore]
fn create_rejects_duplicates_and_foreign_signatures() {
    let s = setup();
    let recipient = wallet(0x10);
    create_auction(&s, &recipient, 1).expect("create");
    assert_eq!(
        create_auction(&s, &recipient, 1).unwrap_err(),
        "auction already exists"
    );

    // A foreign signature under the recipient's address: the message signer is a
    // different key.
    let auction_id = auth::derive_auction_id(s.game.as_slice(), &recipient.address, 2).unwrap();
    let message = auth::auction_message(
        CHAIN,
        &s.game.to_text(),
        &auction_id,
        &auth::Action::Create {
            recipient_nonce: 2,
            duration: DURATION,
            perform_window: PERFORM_WINDOW,
            min_entry: MIN_ENTRY,
        },
    );
    let stranger = wallet(0x66);
    let arg = auction::api::CreateAuctionArg {
        chain: CHAIN.to_string(),
        recipient: serde_bytes::ByteBuf::from(recipient.address.clone()),
        recipient_nonce: 2,
        duration: DURATION,
        perform_window: PERFORM_WINDOW,
        min_entry: MIN_ENTRY,
        signature: serde_bytes::ByteBuf::from(sign(&stranger, message.as_bytes())),
    };
    let (result,): (Result<serde_bytes::ByteBuf, String>,) =
        update(&s.pic, s.game, "create_auction", Encode!(&arg).unwrap());
    assert_eq!(result.unwrap_err(), "bad signature");
}

// ---- lot resolvers -----------------------------------------------------------

#[test]
#[ignore]
fn lot_resolvers_are_stable_and_lot_scoped() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");

    let a1 = get_resolver(&s, &auction_id, &TEXT_A).expect("resolver");
    let a2 = get_resolver(&s, &auction_id, &TEXT_A).expect("resolver");
    let b = get_resolver(&s, &auction_id, &TEXT_B).expect("resolver");
    assert_eq!(a1.len(), 32);
    assert_eq!(a1, a2, "stateless derivation is stable");
    assert_ne!(a1, b, "different texts derive different lot keys");

    // A different auction id shifts every lot key too.
    let other = auth::derive_auction_id(s.game.as_slice(), &recipient.address, 99).unwrap();
    let foreign = get_resolver(&s, &other, &TEXT_A).expect("resolver");
    assert_ne!(a1, foreign);
}

// ---- registration ------------------------------------------------------------

#[test]
#[ignore]
fn registration_admits_a_real_escrow() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));

    let entry = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        1_000_000,
        deadline,
        7,
    );
    let escrow = register_entry(&s, &auction_id, &entry).expect("register");
    assert_eq!(
        escrow, entry.escrow,
        "the canister derived the same address"
    );

    let certified = fetch_lot(&s, &auction_id, &entry.lot_id);
    let lot = lot_state(&certified);
    assert_eq!(lot.text_hash.as_slice(), TEXT_A.as_slice());
    assert_eq!(lot.resolver.to_vec(), entry.resolver);
    assert_eq!(lot.sum, 1_000_000);
    assert_eq!(lot.entries, 1);
    assert!(lot.accepted_at.is_none());
    verify_certified(
        &s,
        &auction::lot_key(CHAIN, &auction_id, &entry.lot_id),
        &certified,
    );

    let entries = list_entries(&s, &auction_id, &entry.lot_id);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].escrow.to_vec(), entry.escrow);
    assert_eq!(entries[0].donor.as_slice(), entry.donor.address.as_slice());
    assert_eq!(entries[0].gross, 1_000_000);
    assert_eq!(entries[0].seq, 1);
    assert!(entries[0].returned.is_none());

    // A top-up by another donor joins the same lot with the next seq.
    let topup = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x22,
        TEXT_A,
        500_000,
        deadline,
        8,
    );
    register_entry(&s, &auction_id, &topup).expect("top-up registers");
    let lot = lot_state(&fetch_lot(&s, &auction_id, &entry.lot_id));
    assert_eq!(lot.sum, 1_500_000);
    assert_eq!(lot.entries, 2);
    let entries = list_entries(&s, &auction_id, &entry.lot_id);
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|e| e.seq == 2));
}

#[test]
#[ignore]
fn registration_rejects_fakes() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let donor = wallet(0x21);
    let resolver = get_resolver(&s, &auction_id, &TEXT_A).expect("resolver");
    let salt = derive_salt(
        &donor.address,
        &recipient.address,
        1_000_000,
        deadline,
        &resolver,
        7,
    );
    let escrow = derive_escrow_address(&salt);
    let honest = || {
        escrow_account_data(
            &donor.address,
            &salt,
            &recipient.address,
            &resolver,
            1_000_000,
            deadline,
            false,
        )
    };
    let try_register = || {
        register(
            &s,
            &auction_id,
            &TEXT_A,
            &donor.address,
            1_000_000,
            deadline,
            7,
        )
    };

    // (a) A phantom: valid declaration, no account on chain.
    assert_eq!(try_register().unwrap_err(), "escrow account does not exist");

    // (b) The account exists but a foreign program owns it.
    plant_account(&s, &escrow, "11111111111111111111111111111111", &honest());
    assert_eq!(
        try_register().unwrap_err(),
        "escrow account is not owned by the factory"
    );

    // (c) The declared donor does not match the account.
    let mut data = honest();
    data[8..40].copy_from_slice(&[0x77; 32]);
    plant_account(&s, &escrow, FACTORY, &data);
    assert_eq!(
        try_register().unwrap_err(),
        "escrow donor does not match the declared birth"
    );

    // (d) The salt does not match: some other birth lives at this address.
    let mut data = honest();
    data[40..72].copy_from_slice(&[0x78; 32]);
    plant_account(&s, &escrow, FACTORY, &data);
    assert_eq!(
        try_register().unwrap_err(),
        "escrow salt does not match the declared birth"
    );

    // (e) A settled escrow is spent money, not a entry.
    let mut data = honest();
    data[187] = 1;
    plant_account(&s, &escrow, FACTORY, &data);
    assert_eq!(try_register().unwrap_err(), "escrow already settled");

    // (f) A price other than the game's derives another address, where no
    // account exists — the wrong-fee escrow simply is not found.
    let mut wrong_fee = Vec::new();
    wrong_fee.extend_from_slice(&donor.address);
    wrong_fee.extend_from_slice(&recipient.address);
    wrong_fee.extend_from_slice(&1_000_000u64.to_le_bytes());
    wrong_fee.extend_from_slice(&(deadline as i64).to_le_bytes());
    wrong_fee.extend_from_slice(&resolver);
    wrong_fee.extend_from_slice(&9_999u16.to_le_bytes());
    wrong_fee.extend_from_slice(&bs58::decode(FEE_WALLET).into_vec().unwrap());
    wrong_fee.extend_from_slice(&7u64.to_le_bytes());
    use sha2::Digest;
    let foreign_salt: [u8; 32] = sha2::Sha256::digest(&wrong_fee).into();
    assert_ne!(foreign_salt, salt, "a foreign fee shifts the salt");

    // After all fakes, the honest account registers.
    plant_account(&s, &escrow, FACTORY, &honest());
    assert_eq!(try_register().unwrap(), escrow);
}

#[test]
#[ignore]
fn registration_applies_the_law() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));

    // The recipient's floor.
    let dust = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        MIN_ENTRY - 1,
        deadline,
        1,
    );
    assert_eq!(
        register_entry(&s, &auction_id, &dust).unwrap_err(),
        "gross below the auction's min_entry"
    );

    // The deadline rule: one second short is short.
    let short = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x22,
        TEXT_A,
        1_000,
        deadline - 1,
        2,
    );
    assert_eq!(
        register_entry(&s, &auction_id, &short).unwrap_err(),
        "escrow deadline too short"
    );

    // Double registration of one escrow.
    let entry = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x23,
        TEXT_A,
        1_000,
        deadline,
        3,
    );
    register_entry(&s, &auction_id, &entry).expect("register");
    assert_eq!(
        register_entry(&s, &auction_id, &entry).unwrap_err(),
        "entry already registered"
    );

    // An unknown auction.
    let ghost = [0x00; 32];
    assert_eq!(
        register(
            &s,
            &ghost,
            &TEXT_A,
            &entry.donor.address,
            1_000,
            deadline,
            4
        )
        .unwrap_err(),
        "unknown auction"
    );

    // Expired bidding: the clock applies first and the registration is
    // rejected; the timer then runs the finale — nothing was accepted, so
    // the auction dies unwon.
    s.pic.advance_time(std::time::Duration::from_secs(DURATION));
    s.pic.tick();
    let late = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x24,
        TEXT_A,
        1_000,
        deadline,
        5,
    );
    assert_eq!(
        register_entry(&s, &auction_id, &late).unwrap_err(),
        "invalid transition"
    );
    assert_eq!(
        auction_state(&fetch_auction(&s, &auction_id)).state,
        StateView::Done { winner: None }
    );
}

#[test]
#[ignore]
fn transport_failure_is_an_error_not_a_write() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let entry = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        1_000_000,
        deadline,
        7,
    );

    set_broken(&s, true);
    let error = register_entry(&s, &auction_id, &entry).unwrap_err();
    assert!(error.contains("no consensus"), "got: {error}");
    assert!(
        list_entries(&s, &auction_id, &entry.lot_id).is_empty(),
        "no write on transport failure"
    );

    // The same declaration registers once the transport heals.
    set_broken(&s, false);
    register_entry(&s, &auction_id, &entry).expect("retry succeeds");
}

// ---- accept and returns --------------------------------------------------------

#[test]
#[ignore]
fn accept_takes_a_lot_into_the_race() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let entry = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        1_000_000,
        deadline,
        7,
    );
    register_entry(&s, &auction_id, &entry).expect("register");

    // A stranger cannot accept; an unknown lot cannot be accepted.
    let stranger = wallet(0x66);
    assert_eq!(
        accept_lot(&s, &auction_id, entry.lot_id, &stranger).unwrap_err(),
        "bad signature"
    );
    assert_eq!(
        accept_lot(&s, &auction_id, [0x0F; 32], &recipient).unwrap_err(),
        "unknown lot"
    );

    accept_lot(&s, &auction_id, entry.lot_id, &recipient).expect("accept");
    let lot = lot_state(&fetch_lot(&s, &auction_id, &entry.lot_id));
    assert!(lot.accepted_at.is_some());
    assert_eq!(
        accept_lot(&s, &auction_id, entry.lot_id, &recipient).unwrap_err(),
        "lot already accepted"
    );

    // After expiry the door is closed — time first.
    let late = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x22,
        TEXT_B,
        2_000_000,
        deadline,
        8,
    );
    register_entry(&s, &auction_id, &late).expect("register");
    s.pic.advance_time(std::time::Duration::from_secs(DURATION));
    s.pic.tick();
    assert_eq!(
        accept_lot(&s, &auction_id, late.lot_id, &recipient).unwrap_err(),
        "invalid transition"
    );
}

#[test]
#[ignore]
fn returned_lot_leaves_the_race_and_takes_no_registrations() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let entry = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        1_000_000,
        deadline,
        7,
    );
    register_entry(&s, &auction_id, &entry).expect("register");

    let stranger = wallet(0x66);
    assert_eq!(
        return_lot(&s, &auction_id, entry.lot_id, &stranger).unwrap_err(),
        "bad signature"
    );

    return_lot(&s, &auction_id, entry.lot_id, &recipient).expect("return");
    let lot = lot_state(&fetch_lot(&s, &auction_id, &entry.lot_id));
    let stamp = lot.returned.expect("returned");
    assert_eq!(stamp.by, auction::ActorView::Recipient);
    assert_eq!(
        return_lot(&s, &auction_id, entry.lot_id, &recipient).unwrap_err(),
        "lot already returned"
    );
    assert_eq!(
        accept_lot(&s, &auction_id, entry.lot_id, &recipient).unwrap_err(),
        "lot already returned"
    );

    // A returned lot takes no further money.
    let topup = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x22,
        TEXT_A,
        500_000,
        deadline,
        8,
    );
    assert_eq!(
        register_entry(&s, &auction_id, &topup).unwrap_err(),
        "lot already returned"
    );
}

#[test]
#[ignore]
fn returned_entry_leaves_the_sum_with_attribution() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let first = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        1_000_000,
        deadline,
        7,
    );
    register_entry(&s, &auction_id, &first).expect("register");
    let second = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x22,
        TEXT_A,
        500_000,
        deadline,
        8,
    );
    register_entry(&s, &auction_id, &second).expect("register");

    let stranger = wallet(0x66);
    assert_eq!(
        return_entry(&s, &auction_id, &second.escrow, &stranger).unwrap_err(),
        "bad signature"
    );
    assert_eq!(
        return_entry(&s, &auction_id, &[0x0F; 32], &recipient).unwrap_err(),
        "unknown entry"
    );

    return_entry(&s, &auction_id, &second.escrow, &recipient).expect("return");
    let certified = fetch_lot(&s, &auction_id, &first.lot_id);
    let lot = lot_state(&certified);
    assert_eq!(lot.sum, 1_000_000, "the sum lost the returned entry");
    assert_eq!(lot.entries, 2, "the count keeps history");
    assert!(lot.returned.is_none(), "the lot itself stays in the race");
    verify_certified(
        &s,
        &auction::lot_key(CHAIN, &auction_id, &first.lot_id),
        &certified,
    );
    let entries = list_entries(&s, &auction_id, &first.lot_id);
    let returned = entries
        .iter()
        .find(|e| e.escrow.as_slice() == second.escrow.as_slice())
        .expect("entry listed");
    let stamp = returned.returned.expect("stamped");
    assert_eq!(stamp.by, auction::ActorView::Recipient);

    assert_eq!(
        return_entry(&s, &auction_id, &second.escrow, &recipient).unwrap_err(),
        "entry already returned"
    );

    // Entries of a returned lot are already covered by the lot's cancel.
    return_lot(&s, &auction_id, first.lot_id, &recipient).expect("return lot");
    assert_eq!(
        return_entry(&s, &auction_id, &first.escrow, &recipient).unwrap_err(),
        "lot already returned"
    );
}

#[test]
#[ignore]
fn recipient_cancel_kills_the_auction_in_bidding_only() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let entry = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        1_000_000,
        deadline,
        7,
    );
    register_entry(&s, &auction_id, &entry).expect("register");

    let stranger = wallet(0x66);
    assert_eq!(
        cancel_auction(&s, &auction_id, &stranger).unwrap_err(),
        "bad signature"
    );

    cancel_auction(&s, &auction_id, &recipient).expect("cancel");
    let record = auction_state(&fetch_auction(&s, &auction_id));
    assert_eq!(record.state, StateView::Done { winner: None });

    // The dead auction takes nothing.
    let late = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x22,
        TEXT_B,
        500_000,
        deadline,
        8,
    );
    assert_eq!(
        register_entry(&s, &auction_id, &late).unwrap_err(),
        "invalid transition"
    );
    assert_eq!(
        accept_lot(&s, &auction_id, entry.lot_id, &recipient).unwrap_err(),
        "invalid transition"
    );
    assert_eq!(
        cancel_auction(&s, &auction_id, &recipient).unwrap_err(),
        "invalid transition"
    );

    // Expired bidding closes the cancel door too, on a fresh auction; the
    // timer then finalizes the lotless auction into an unwon Done.
    let auction_b = create_auction(&s, &recipient, 2).expect("create");
    s.pic.advance_time(std::time::Duration::from_secs(DURATION));
    s.pic.tick();
    assert_eq!(
        cancel_auction(&s, &auction_b, &recipient).unwrap_err(),
        "invalid transition"
    );
    assert_eq!(
        auction_state(&fetch_auction(&s, &auction_b)).state,
        StateView::Done { winner: None }
    );
}

// ---- upgrade -------------------------------------------------------------------

#[test]
#[ignore]
fn upgrade_preserves_records_byte_for_byte() {
    let s = setup();
    let recipient = wallet(0x10);
    let auction_id = create_auction(&s, &recipient, 1).expect("create");
    let deadline = good_deadline(created_at(&s, &auction_id));
    let entry = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x21,
        TEXT_A,
        1_000_000,
        deadline,
        7,
    );
    register_entry(&s, &auction_id, &entry).expect("register");
    accept_lot(&s, &auction_id, entry.lot_id, &recipient).expect("accept");

    let auction_before = fetch_auction(&s, &auction_id).data;
    let lot_before = fetch_lot(&s, &auction_id, &entry.lot_id).data;

    s.pic
        .upgrade_canister(s.game, game_wasm(), Encode!().unwrap(), None)
        .expect("upgrade");

    let auction_after = fetch_auction(&s, &auction_id);
    assert_eq!(auction_before, auction_after.data, "auction bytes intact");
    let lot_after = fetch_lot(&s, &auction_id, &entry.lot_id);
    assert_eq!(lot_before, lot_after.data, "lot bytes intact");
    // The certified tree was rebuilt from stable truth.
    verify_certified_auction(&s, &auction_id, &auction_after);
    verify_certified(
        &s,
        &auction::lot_key(CHAIN, &auction_id, &entry.lot_id),
        &lot_after,
    );

    // The registry keeps working: the seq counter survived the upgrade.
    let topup = plant_entry(
        &s,
        &auction_id,
        &recipient,
        0x22,
        TEXT_A,
        500_000,
        deadline,
        8,
    );
    register_entry(&s, &auction_id, &topup).expect("register after upgrade");
    let entries = list_entries(&s, &auction_id, &entry.lot_id);
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|e| e.seq == 2), "seq is monotonic");
}
