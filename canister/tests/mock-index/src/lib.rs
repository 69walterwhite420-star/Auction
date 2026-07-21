//! Test double of crown-index: the same `get_reputation` query over a book
//! seeded by an update — something the real canister never allows, which is
//! exactly why this mock lives outside the trusted repositories.
//! `set_broken(true)` makes the book refuse to answer, the failure a vote
//! must survive without losing its right to be cast.

use std::cell::RefCell;
use std::collections::BTreeMap;

use candid::Nat;
use serde_bytes::ByteBuf;

thread_local! {
    static BOOK: RefCell<BTreeMap<(String, Vec<u8>, Vec<u8>), u128>> =
        const { RefCell::new(BTreeMap::new()) };
    static BROKEN: RefCell<bool> = const { RefCell::new(false) };
}

#[ic_cdk::update]
fn set_reputation(chain: String, donor: ByteBuf, recipient: ByteBuf, value: u128) {
    BOOK.with_borrow_mut(|book| {
        book.insert((chain, donor.into_vec(), recipient.into_vec()), value);
    });
}

#[ic_cdk::update]
fn set_broken(broken: bool) {
    BROKEN.with_borrow_mut(|cell| *cell = broken);
}

#[ic_cdk::query]
fn get_reputation(chain: String, donor: ByteBuf, recipient: ByteBuf) -> Nat {
    if BROKEN.with_borrow(|cell| *cell) {
        ic_cdk::trap("mock crown-index is down");
    }
    BOOK.with_borrow(|book| {
        Nat::from(
            book.get(&(chain, donor.into_vec(), recipient.into_vec()))
                .copied()
                .unwrap_or(0),
        )
    })
}
