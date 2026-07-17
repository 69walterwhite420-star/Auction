//! auction-logic: the pure law of the game (docs/game-spec.md §5, §9, §10).
//!
//! Zero dependencies, no I/O, no clock — time arrives as an argument. Lots
//! and entries are opaque ids and numbers; this crate knows nothing about
//! chains, cryptography or the canister hosting it.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

pub mod auction;
pub mod finale;
pub mod verdict;
pub mod vote;

pub use auction::{
    Action, Actor, Auction, CreateError, DEADLINE_MARGIN, LOGIC_VERSION, MAX_DURATION,
    MIN_DURATION, Profile, State, StepError, create, step,
};
pub use finale::{Entry, Standing, beats, standing, winner};
pub use verdict::{Outcome, verdict};
pub use vote::{Choice, MIN_VOTE_WEIGHT, Vote, Voter};
