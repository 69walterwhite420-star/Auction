//! auction-logic: the pure law of the game — state machine, bidding finale,
//! vote counting, the deadline rule (docs/game-spec.md §5, §9, §10).
//!
//! Zero dependencies. Time, registry snapshots and vote weights arrive as
//! arguments; lots and entries are opaque ids and numbers. Stage G0 pins the
//! boundaries only — the law lands in G1.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
