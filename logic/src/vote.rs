//! Votes: an opaque voter, a binary choice, a book weight (docs/game-spec.md §10).

/// Minimal book value to vote, in minor units of reputation (the book is
/// denominated in USDC, 6 decimals). Enforced by the machine, not by the
/// verdict rule: recorded votes always count.
pub const MIN_VOTE_WEIGHT: u128 = 100_000;

/// Opaque wallet bytes on the chain the escrows live on. The canister
/// normalizes encodings; this crate only compares them for equality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voter(pub Vec<u8>);

/// What the voter asserts about the winning condition the recipient claimed done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Done,
    NotDone,
}

/// One recorded vote. `weight` is book[(chain, voter, recipient)] at the moment
/// the vote was processed — there is no snapshot (game-spec §10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vote {
    pub voter: Voter,
    pub choice: Choice,
    pub weight: u128,
}
