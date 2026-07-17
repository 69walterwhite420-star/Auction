//! The auction state machine: one diagram, time first (docs/game-spec.md §5).
//!
//! Every `step` applies due time transitions before the action, so a late
//! canister timer can never let an action sneak past an expired clock.
//! `Tick` is the identity action: pure time, nothing else. A failed action
//! therefore still applies due time transitions — the caller relies on it.
//!
//! Registry-level moves — registering an entry, accepting or returning a
//! lot or an entry — transition nothing here but are drawn as actions
//! anyway: their state and time law lives in this one exhaustive match,
//! the caller only records the allowed outcome in its registry.
//!
//! FINALE_DUE is the bounded-scan window: the clock closed bidding and the
//! caller folds its registry into standings (finale.rs), a portion per
//! tick, then applies `Finale`. Only `Finale` and `Tick` are drawn there:
//! returns wait out the scan, so the composition being measured never
//! shifts under it.

use crate::verdict::{Outcome, verdict};
use crate::vote::{MIN_VOTE_WEIGHT, Vote};

/// Version of the game rules. Bumped only by a conscious change to the
/// machine, the finale or the verdict rule; the canister reports it via
/// query.
pub const LOGIC_VERSION: u32 = 1;

/// All times are unix seconds; time is always an argument, never a syscall.
/// Both KM handles — the bidding window and the performance window — share
/// the same range.
pub const MIN_DURATION: u64 = 60; // 1 minute
pub const MAX_DURATION: u64 = 2_592_000; // 30 days

/// Slack between the last honest outcome and the escrow's refund() door
/// (game-spec §9): the registration gate keeps refund() behind the finale,
/// the performance and the vote.
pub const DEADLINE_MARGIN: u64 = 259_200; // 72 hours

/// The profile values an auction snapshots at birth (docs/game-spec.md §11).
/// Changing the profile never touches auctions already created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub voting_period: u64,
}

/// Who signed a return. The caller authenticates the signature; this crate
/// only applies the actor's window of the law.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    Km,
    Operator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Bids register, the KM accepts and returns; the leaderboard grows.
    Bidding,
    /// The clock closed bidding; the caller is computing the finale over
    /// its frozen registry.
    FinaleDue,
    /// A winner exists; the KM performs the winning condition until
    /// `created_at + duration + perform_window`.
    Performing,
    /// The KM claimed the condition performed; reputation holders vote.
    Voting { started_at: u64 },
    /// Terminal. The winner lot's outcome; `None` when no winner ever
    /// existed — no accepted lots, zero sums, or the KM cancelled the
    /// auction. Every non-winner lot resolves to cancel by the auction
    /// rule (game-spec §8).
    Done { winner: Option<Outcome> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Auction {
    pub created_at: u64,
    /// The bidding window, from creation.
    pub duration: u64,
    /// The KM's window to perform the winning condition, from the end of
    /// bidding.
    pub perform_window: u64,
    /// Snapshot of the profile at birth; an auction carries its own clock
    /// forever.
    pub voting_period: u64,
    /// The KM's floor for one entry's gross; 0 = only the shape's floor.
    pub min_bid: u64,
    pub state: State,
    /// Non-empty only from VOTING on; published forever after the verdict.
    pub votes: Vec<Vote>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// An entry registers: the caller verified the escrow on chain, this
    /// arm applies the KM's floor and the deadline rule (game-spec §9).
    Register { gross: u64, deadline: i64 },
    /// The KM accepts a lot: its text goes public, the lot enters the race.
    AcceptLot,
    /// The KM or the operator returns a whole lot (BIDDING only; the winner
    /// goes through its own doors below).
    ReturnLot,
    /// One entry is returned. `in_winner_lot` is a registry fact supplied
    /// by the caller; before the finale every lot passes as the race is
    /// still open.
    ReturnEntry { by: Actor, in_winner_lot: bool },
    /// The KM aborts the whole auction: cancel for every lot.
    KmCancel,
    /// The registry scan finished: a winner exists or not.
    Finale { winner: bool },
    /// The KM claims the winning condition performed; voting opens.
    Ready,
    /// The KM returns the winner before "ready": the auction dies
    /// unsettled — there is no second place (game-spec §5).
    KmReturnWinner,
    /// The operator returns the winner; drawn in VOTING too (§13).
    OperatorReturnWinner,
    /// A reputation holder votes on the winner.
    Vote(Vote),
    /// Pure time: due transitions only.
    Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateError {
    DurationOutOfRange,
    PerformWindowOutOfRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepError {
    /// The transition is not drawn on the diagram (or the clock closed it).
    InvalidTransition,
    BelowMinBid,
    DeadlineTooShort,
    WeightBelowThreshold,
    DuplicateVoter,
    Overflow,
}

/// Validates the KM's handles against the rules and births the auction in
/// BIDDING. On `Err` no auction exists. `min_bid` is any u64: 0 legally
/// leaves only the shape's own floor.
pub fn create(
    now: u64,
    profile: &Profile,
    duration: u64,
    perform_window: u64,
    min_bid: u64,
) -> Result<Auction, CreateError> {
    if !(MIN_DURATION..=MAX_DURATION).contains(&duration) {
        return Err(CreateError::DurationOutOfRange);
    }
    if !(MIN_DURATION..=MAX_DURATION).contains(&perform_window) {
        return Err(CreateError::PerformWindowOutOfRange);
    }
    Ok(Auction {
        created_at: now,
        duration,
        perform_window,
        voting_period: profile.voting_period,
        min_bid,
        state: State::Bidding,
        votes: Vec::new(),
    })
}

/// Applies one action at time `now`. Due time transitions happen first and
/// persist even when the action itself fails; `Done` is absorbing.
pub fn step(auction: &mut Auction, action: Action, now: u64) -> Result<(), StepError> {
    advance(auction, now)?;
    let next = match (auction.state.clone(), action) {
        // --- bidding: the registry fills up ------------------------------
        (State::Bidding, Action::Register { gross, deadline }) => {
            if gross < auction.min_bid {
                return Err(StepError::BelowMinBid);
            }
            let floor = deadline_floor(auction)?;
            if u64::try_from(deadline).unwrap_or(0) < floor {
                return Err(StepError::DeadlineTooShort);
            }
            None
        }
        (State::Bidding, Action::AcceptLot)
        | (State::Bidding, Action::ReturnLot)
        | (State::Bidding, Action::ReturnEntry { .. }) => None,
        (State::Bidding, Action::KmCancel) => Some(State::Done { winner: None }),

        // --- the finale: the caller finished folding its registry --------
        (State::FinaleDue, Action::Finale { winner: true }) => Some(State::Performing),
        (State::FinaleDue, Action::Finale { winner: false }) => Some(State::Done { winner: None }),

        // --- performing and the winner's doors ---------------------------
        (State::Performing, Action::Ready) => Some(State::Voting { started_at: now }),
        (State::Performing, Action::KmReturnWinner)
        | (State::Performing, Action::OperatorReturnWinner)
        | (State::Voting { .. }, Action::OperatorReturnWinner) => Some(State::Done {
            winner: Some(Outcome::Cancel),
        }),
        (
            State::Performing,
            Action::ReturnEntry {
                by: _,
                in_winner_lot: true,
            },
        )
        | (
            State::Voting { .. },
            Action::ReturnEntry {
                by: Actor::Operator,
                in_winner_lot: true,
            },
        ) => None,

        // --- voting -------------------------------------------------------
        (State::Voting { .. }, Action::Vote(vote)) => {
            if vote.weight < MIN_VOTE_WEIGHT {
                return Err(StepError::WeightBelowThreshold);
            }
            if auction.votes.iter().any(|v| v.voter == vote.voter) {
                return Err(StepError::DuplicateVoter);
            }
            auction.votes.push(vote);
            None
        }

        // --- pure time ------------------------------------------------------
        (State::Bidding, Action::Tick)
        | (State::FinaleDue, Action::Tick)
        | (State::Performing, Action::Tick)
        | (State::Voting { .. }, Action::Tick)
        | (State::Done { .. }, Action::Tick) => None,

        // --- everything not drawn on the diagram (docs/game-spec.md §5) ----
        (State::Bidding, Action::Finale { .. })
        | (State::Bidding, Action::Ready)
        | (State::Bidding, Action::KmReturnWinner)
        | (State::Bidding, Action::OperatorReturnWinner)
        | (State::Bidding, Action::Vote(_))
        | (State::FinaleDue, Action::Register { .. })
        | (State::FinaleDue, Action::AcceptLot)
        | (State::FinaleDue, Action::ReturnLot)
        | (State::FinaleDue, Action::ReturnEntry { .. })
        | (State::FinaleDue, Action::KmCancel)
        | (State::FinaleDue, Action::Ready)
        | (State::FinaleDue, Action::KmReturnWinner)
        | (State::FinaleDue, Action::OperatorReturnWinner)
        | (State::FinaleDue, Action::Vote(_))
        | (State::Performing, Action::Register { .. })
        | (State::Performing, Action::AcceptLot)
        | (State::Performing, Action::ReturnLot)
        | (
            State::Performing,
            Action::ReturnEntry {
                by: _,
                in_winner_lot: false,
            },
        )
        | (State::Performing, Action::KmCancel)
        | (State::Performing, Action::Finale { .. })
        | (State::Performing, Action::Vote(_))
        | (State::Voting { .. }, Action::Register { .. })
        | (State::Voting { .. }, Action::AcceptLot)
        | (State::Voting { .. }, Action::ReturnLot)
        | (
            State::Voting { .. },
            Action::ReturnEntry {
                by: Actor::Km,
                in_winner_lot: _,
            },
        )
        | (
            State::Voting { .. },
            Action::ReturnEntry {
                by: Actor::Operator,
                in_winner_lot: false,
            },
        )
        | (State::Voting { .. }, Action::KmCancel)
        | (State::Voting { .. }, Action::Finale { .. })
        | (State::Voting { .. }, Action::Ready)
        | (State::Voting { .. }, Action::KmReturnWinner)
        | (State::Done { .. }, Action::Register { .. })
        | (State::Done { .. }, Action::AcceptLot)
        | (State::Done { .. }, Action::ReturnLot)
        | (State::Done { .. }, Action::ReturnEntry { .. })
        | (State::Done { .. }, Action::KmCancel)
        | (State::Done { .. }, Action::Finale { .. })
        | (State::Done { .. }, Action::Ready)
        | (State::Done { .. }, Action::KmReturnWinner)
        | (State::Done { .. }, Action::OperatorReturnWinner)
        | (State::Done { .. }, Action::Vote(_)) => {
            return Err(StepError::InvalidTransition);
        }
    };
    if let Some(state) = next {
        auction.state = state;
    }
    Ok(())
}

/// Due time transitions: bidding expiry freezes the registry for the
/// finale, the performance deadline cancels the winner, the end of the
/// voting period tallies the verdict. `Done` never changes again;
/// FINALE_DUE waits for the caller's `Finale`, not for the clock.
fn advance(auction: &mut Auction, now: u64) -> Result<(), StepError> {
    match auction.state.clone() {
        State::Bidding => {
            let expiry = auction
                .created_at
                .checked_add(auction.duration)
                .ok_or(StepError::Overflow)?;
            if now >= expiry {
                auction.state = State::FinaleDue;
            }
            Ok(())
        }
        State::FinaleDue => Ok(()),
        State::Performing => {
            let deadline = auction
                .created_at
                .checked_add(auction.duration)
                .and_then(|t| t.checked_add(auction.perform_window))
                .ok_or(StepError::Overflow)?;
            if now >= deadline {
                auction.state = State::Done {
                    winner: Some(Outcome::Cancel),
                };
            }
            Ok(())
        }
        State::Voting { started_at } => {
            let end = started_at
                .checked_add(auction.voting_period)
                .ok_or(StepError::Overflow)?;
            if now >= end {
                // Infallible: a voting auction always finalizes at period
                // end — even an overflowing tally decides (Cancel), never
                // strands (verdict.rs).
                auction.state = State::Done {
                    winner: Some(verdict(&auction.votes)),
                };
            }
            Ok(())
        }
        State::Done { .. } => Ok(()),
    }
}

/// The escrow deadline rule (game-spec §9): refund() must sit behind every
/// honest outcome — the finale, the performance window and the vote — with
/// the margin on top.
fn deadline_floor(auction: &Auction) -> Result<u64, StepError> {
    auction
        .created_at
        .checked_add(auction.duration)
        .and_then(|t| t.checked_add(auction.perform_window))
        .and_then(|t| t.checked_add(auction.voting_period))
        .and_then(|t| t.checked_add(DEADLINE_MARGIN))
        .ok_or(StepError::Overflow)
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
    use proptest::prelude::*;

    use super::*;
    use crate::vote::{Choice, Voter};

    const T0: u64 = 1_700_000_000;
    const DURATION: u64 = 86_400; // 1 day of bidding
    const PERFORM_WINDOW: u64 = 43_200; // half a day to perform
    const VOTING_PERIOD: u64 = 3_600;
    const MIN_BID: u64 = 1_000;

    const BIDDING_END: u64 = T0 + DURATION;
    const PERFORM_DEADLINE: u64 = BIDDING_END + PERFORM_WINDOW;
    const GOOD_DEADLINE: i64 =
        (T0 + DURATION + PERFORM_WINDOW + VOTING_PERIOD + DEADLINE_MARGIN) as i64;

    fn profile() -> Profile {
        Profile {
            voting_period: VOTING_PERIOD,
        }
    }

    fn auction() -> Auction {
        create(T0, &profile(), DURATION, PERFORM_WINDOW, MIN_BID).unwrap()
    }

    fn register() -> Action {
        Action::Register {
            gross: MIN_BID,
            deadline: GOOD_DEADLINE,
        }
    }

    fn vote(voter: u8, choice: Choice, weight: u128) -> Vote {
        Vote {
            voter: Voter(vec![voter]),
            choice,
            weight,
        }
    }

    /// Bidding expired, finale applied with a winner: PERFORMING.
    fn performing() -> Auction {
        let mut a = auction();
        step(&mut a, Action::Finale { winner: true }, BIDDING_END).unwrap();
        assert_eq!(a.state, State::Performing);
        a
    }

    /// The KM claimed "ready" one second into PERFORMING: VOTING.
    fn voting() -> Auction {
        let mut a = performing();
        step(&mut a, Action::Ready, BIDDING_END + 1).unwrap();
        assert_eq!(
            a.state,
            State::Voting {
                started_at: BIDDING_END + 1
            }
        );
        a
    }

    // --- creation -----------------------------------------------------

    #[test]
    fn create_validates_duration() {
        for bad in [MIN_DURATION - 1, MAX_DURATION + 1] {
            assert_eq!(
                create(T0, &profile(), bad, PERFORM_WINDOW, MIN_BID),
                Err(CreateError::DurationOutOfRange)
            );
        }
        assert!(create(T0, &profile(), MIN_DURATION, PERFORM_WINDOW, MIN_BID).is_ok());
        assert!(create(T0, &profile(), MAX_DURATION, PERFORM_WINDOW, MIN_BID).is_ok());
    }

    #[test]
    fn create_validates_perform_window() {
        for bad in [MIN_DURATION - 1, MAX_DURATION + 1] {
            assert_eq!(
                create(T0, &profile(), DURATION, bad, MIN_BID),
                Err(CreateError::PerformWindowOutOfRange)
            );
        }
        assert!(create(T0, &profile(), DURATION, MIN_DURATION, 0).is_ok());
    }

    #[test]
    fn auction_snapshots_profile() {
        let a = auction();
        assert_eq!(a.voting_period, VOTING_PERIOD);
        assert_eq!(a.min_bid, MIN_BID);
        assert_eq!(a.state, State::Bidding);
        assert!(a.votes.is_empty());
    }

    // --- registration gates ---------------------------------------------

    #[test]
    fn register_applies_the_km_floor() {
        let mut a = auction();
        assert_eq!(
            step(
                &mut a,
                Action::Register {
                    gross: MIN_BID - 1,
                    deadline: GOOD_DEADLINE,
                },
                T0 + 1,
            ),
            Err(StepError::BelowMinBid)
        );
        assert!(step(&mut a, register(), T0 + 1).is_ok());
    }

    #[test]
    fn register_applies_the_deadline_rule() {
        // ok ⇔ deadline ≥ created + duration + perform_window +
        // voting_period + margin; the boundary itself passes.
        let mut a = auction();
        assert_eq!(
            step(
                &mut a,
                Action::Register {
                    gross: MIN_BID,
                    deadline: GOOD_DEADLINE - 1,
                },
                T0 + 1,
            ),
            Err(StepError::DeadlineTooShort)
        );
        assert_eq!(
            step(
                &mut a,
                Action::Register {
                    gross: MIN_BID,
                    deadline: -1,
                },
                T0 + 1,
            ),
            Err(StepError::DeadlineTooShort)
        );
        assert!(step(&mut a, register(), T0 + 1).is_ok());
    }

    #[test]
    fn deadline_floor_overflow_is_an_error_not_a_pass() {
        let mut a = auction();
        a.created_at = u64::MAX - DURATION;
        let now = a.created_at;
        assert_eq!(
            step(
                &mut a,
                Action::Register {
                    gross: MIN_BID,
                    deadline: i64::MAX,
                },
                now,
            ),
            Err(StepError::Overflow)
        );
    }

    // --- the drawn diagram ---------------------------------------------

    #[test]
    fn bidding_registry_moves_are_drawn() {
        let mut a = auction();
        for action in [
            register(),
            Action::AcceptLot,
            Action::ReturnLot,
            Action::ReturnEntry {
                by: Actor::Km,
                in_winner_lot: false,
            },
            Action::ReturnEntry {
                by: Actor::Operator,
                in_winner_lot: false,
            },
        ] {
            step(&mut a, action, T0 + 1).unwrap();
            assert_eq!(a.state, State::Bidding);
        }
    }

    #[test]
    fn km_cancel_kills_the_whole_auction_from_bidding_only() {
        let mut a = auction();
        step(&mut a, Action::KmCancel, T0 + 1).unwrap();
        assert_eq!(a.state, State::Done { winner: None });

        // Expired bidding closes the door: the clock applies first.
        let mut a = auction();
        assert_eq!(
            step(&mut a, Action::KmCancel, BIDDING_END),
            Err(StepError::InvalidTransition)
        );
        assert_eq!(a.state, State::FinaleDue);
    }

    #[test]
    fn expiry_freezes_the_registry_for_the_scan() {
        // Every registry move is rejected in FINALE_DUE, and the due time
        // transition persists even though the action failed.
        for action in [
            register(),
            Action::AcceptLot,
            Action::ReturnLot,
            Action::ReturnEntry {
                by: Actor::Operator,
                in_winner_lot: true,
            },
        ] {
            let mut a = auction();
            assert_eq!(
                step(&mut a, action, BIDDING_END),
                Err(StepError::InvalidTransition)
            );
            assert_eq!(a.state, State::FinaleDue);
        }
    }

    #[test]
    fn finale_picks_the_road() {
        let mut a = auction();
        step(&mut a, Action::Finale { winner: true }, BIDDING_END + 5).unwrap();
        assert_eq!(a.state, State::Performing);

        let mut a = auction();
        step(&mut a, Action::Finale { winner: false }, BIDDING_END + 5).unwrap();
        assert_eq!(a.state, State::Done { winner: None });
    }

    #[test]
    fn ready_opens_voting_until_the_last_second() {
        let mut a = performing();
        step(&mut a, Action::Ready, PERFORM_DEADLINE - 1).unwrap();
        assert_eq!(
            a.state,
            State::Voting {
                started_at: PERFORM_DEADLINE - 1
            }
        );

        // At the deadline the clock cancels first.
        let mut a = performing();
        assert_eq!(
            step(&mut a, Action::Ready, PERFORM_DEADLINE),
            Err(StepError::InvalidTransition)
        );
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Cancel)
            }
        );
    }

    #[test]
    fn perform_expiry_cancels_the_winner() {
        let mut a = performing();
        step(&mut a, Action::Tick, PERFORM_DEADLINE).unwrap();
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Cancel)
            }
        );
    }

    #[test]
    fn km_returns_the_winner_before_ready_only() {
        // Before "ready": the auction dies unsettled, no second place.
        let mut a = performing();
        step(&mut a, Action::KmReturnWinner, BIDDING_END + 2).unwrap();
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Cancel)
            }
        );

        // "Ready" hands the work to the vote and closes the KM's door.
        let mut a = voting();
        assert_eq!(
            step(&mut a, Action::KmReturnWinner, BIDDING_END + 2),
            Err(StepError::InvalidTransition)
        );
    }

    #[test]
    fn operator_returns_the_winner_in_performing_and_voting() {
        let mut a = performing();
        step(&mut a, Action::OperatorReturnWinner, BIDDING_END + 2).unwrap();
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Cancel)
            }
        );

        let mut a = voting();
        step(
            &mut a,
            Action::Vote(vote(0, Choice::Done, 200_000)),
            BIDDING_END + 2,
        )
        .unwrap();
        step(&mut a, Action::OperatorReturnWinner, BIDDING_END + 3).unwrap();
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Cancel)
            }
        );
        // Recorded votes stay published.
        assert_eq!(a.votes.len(), 1);
    }

    #[test]
    fn operator_never_beats_the_clock() {
        // A voting window that already ended tallies first; the operator
        // cannot flip the tallied settle.
        let mut a = voting();
        step(
            &mut a,
            Action::Vote(vote(0, Choice::Done, 200_000)),
            BIDDING_END + 2,
        )
        .unwrap();
        let end = BIDDING_END + 1 + VOTING_PERIOD;
        assert_eq!(
            step(&mut a, Action::OperatorReturnWinner, end),
            Err(StepError::InvalidTransition)
        );
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Settle)
            }
        );
    }

    #[test]
    fn return_entry_windows_by_actor() {
        // PERFORMING: both actors may return a winner-lot entry; a lot that
        // lost is already decided and out of reach.
        for by in [Actor::Km, Actor::Operator] {
            let mut a = performing();
            step(
                &mut a,
                Action::ReturnEntry {
                    by,
                    in_winner_lot: true,
                },
                BIDDING_END + 2,
            )
            .unwrap();
            assert_eq!(a.state, State::Performing);
            assert_eq!(
                step(
                    &mut a,
                    Action::ReturnEntry {
                        by,
                        in_winner_lot: false,
                    },
                    BIDDING_END + 3,
                ),
                Err(StepError::InvalidTransition)
            );
        }

        // VOTING: only the operator, only the winner lot.
        let mut a = voting();
        step(
            &mut a,
            Action::ReturnEntry {
                by: Actor::Operator,
                in_winner_lot: true,
            },
            BIDDING_END + 2,
        )
        .unwrap();
        assert_eq!(
            step(
                &mut a,
                Action::ReturnEntry {
                    by: Actor::Km,
                    in_winner_lot: true,
                },
                BIDDING_END + 3,
            ),
            Err(StepError::InvalidTransition)
        );
        assert_eq!(
            step(
                &mut a,
                Action::ReturnEntry {
                    by: Actor::Operator,
                    in_winner_lot: false,
                },
                BIDDING_END + 3,
            ),
            Err(StepError::InvalidTransition)
        );
    }

    // --- votes ----------------------------------------------------------

    #[test]
    fn vote_below_weight_threshold_is_rejected() {
        let mut a = voting();
        assert_eq!(
            step(
                &mut a,
                Action::Vote(vote(0, Choice::Done, MIN_VOTE_WEIGHT - 1)),
                BIDDING_END + 2,
            ),
            Err(StepError::WeightBelowThreshold)
        );
        assert!(a.votes.is_empty());
    }

    #[test]
    fn duplicate_voter_is_rejected_and_counts_once() {
        let mut a = voting();
        step(
            &mut a,
            Action::Vote(vote(0, Choice::Done, 200_000)),
            BIDDING_END + 2,
        )
        .unwrap();
        assert_eq!(
            step(
                &mut a,
                Action::Vote(vote(0, Choice::NotDone, 900_000)),
                BIDDING_END + 3,
            ),
            Err(StepError::DuplicateVoter)
        );
        assert_eq!(a.votes.len(), 1);
        assert_eq!(a.votes[0], vote(0, Choice::Done, 200_000));
    }

    #[test]
    fn vote_outside_voting_is_rejected() {
        let mut a = auction();
        assert_eq!(
            step(&mut a, Action::Vote(vote(0, Choice::Done, 200_000)), T0 + 1,),
            Err(StepError::InvalidTransition)
        );
        let mut a = performing();
        assert_eq!(
            step(
                &mut a,
                Action::Vote(vote(0, Choice::Done, 200_000)),
                BIDDING_END + 2,
            ),
            Err(StepError::InvalidTransition)
        );
    }

    #[test]
    fn vote_at_period_end_is_too_late() {
        let mut a = voting();
        assert_eq!(
            step(
                &mut a,
                Action::Vote(vote(0, Choice::Done, 200_000)),
                BIDDING_END + 1 + VOTING_PERIOD,
            ),
            Err(StepError::InvalidTransition)
        );
        // The tally happened first: silence cancels.
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Cancel)
            }
        );
    }

    #[test]
    fn voting_tallies_at_period_end() {
        let mut a = voting();
        step(
            &mut a,
            Action::Vote(vote(0, Choice::Done, 200_000)),
            BIDDING_END + 2,
        )
        .unwrap();
        step(&mut a, Action::Tick, BIDDING_END + 1 + VOTING_PERIOD).unwrap();
        assert_eq!(
            a.state,
            State::Done {
                winner: Some(Outcome::Settle)
            }
        );
    }

    // --- property tests --------------------------------------------------

    /// An arbitrary action paired with an offset from T0, so sequences can
    /// hit every state of the machine. Register payloads are always valid:
    /// their gates have dedicated unit tests.
    fn actions() -> impl Strategy<Value = Vec<(Action, u64)>> {
        proptest::collection::vec(
            (
                prop_oneof![
                    Just(ActionKind::Register),
                    Just(ActionKind::AcceptLot),
                    Just(ActionKind::ReturnLot),
                    Just(ActionKind::ReturnEntry),
                    Just(ActionKind::KmCancel),
                    Just(ActionKind::Finale),
                    Just(ActionKind::Ready),
                    Just(ActionKind::KmReturnWinner),
                    Just(ActionKind::OperatorReturnWinner),
                    Just(ActionKind::Vote),
                    Just(ActionKind::Tick),
                ],
                any::<u8>(),
                any::<bool>(),
                any::<bool>(),
                MIN_VOTE_WEIGHT..=u128::from(u64::MAX),
                0u64..=(DURATION + PERFORM_WINDOW + VOTING_PERIOD) * 2,
            ),
            0..24,
        )
        .prop_map(|entries| {
            entries
                .into_iter()
                .map(|(kind, voter, flag_a, flag_b, weight, offset)| {
                    let actor = if flag_a { Actor::Km } else { Actor::Operator };
                    let action = match kind {
                        ActionKind::Register => register(),
                        ActionKind::AcceptLot => Action::AcceptLot,
                        ActionKind::ReturnLot => Action::ReturnLot,
                        ActionKind::ReturnEntry => Action::ReturnEntry {
                            by: actor,
                            in_winner_lot: flag_b,
                        },
                        ActionKind::KmCancel => Action::KmCancel,
                        ActionKind::Finale => Action::Finale { winner: flag_b },
                        ActionKind::Ready => Action::Ready,
                        ActionKind::KmReturnWinner => Action::KmReturnWinner,
                        ActionKind::OperatorReturnWinner => Action::OperatorReturnWinner,
                        ActionKind::Vote => {
                            let choice = if flag_b {
                                Choice::Done
                            } else {
                                Choice::NotDone
                            };
                            Action::Vote(vote(voter, choice, weight))
                        }
                        ActionKind::Tick => Action::Tick,
                    };
                    (action, T0 + offset)
                })
                .collect()
        })
    }

    #[derive(Clone, Copy, Debug)]
    enum ActionKind {
        Register,
        AcceptLot,
        ReturnLot,
        ReturnEntry,
        KmCancel,
        Finale,
        Ready,
        KmReturnWinner,
        OperatorReturnWinner,
        Vote,
        Tick,
    }

    proptest! {
        // Unreachability: actions not drawn for the current state always
        // error and never mutate anything but due time transitions.
        #[test]
        fn undrawn_transitions_are_unreachable(seq in actions()) {
            let mut a = auction();
            for (action, now) in seq {
                let before = a.clone();
                let result = step(&mut a, action.clone(), now);
                // The diagram, restated independently of `step` (wildcards
                // are fine here: the machine itself matches exhaustively).
                let drawn = match (&before.state, &action) {
                    (State::Bidding, Action::Register { .. })
                    | (State::Bidding, Action::AcceptLot)
                    | (State::Bidding, Action::ReturnLot)
                    | (State::Bidding, Action::ReturnEntry { .. })
                    | (State::Bidding, Action::KmCancel) => now < BIDDING_END,
                    (State::FinaleDue, Action::Finale { .. }) => true,
                    // Time first: a Finale arriving after expiry lands in
                    // the FINALE_DUE the clock just entered.
                    (State::Bidding, Action::Finale { .. }) => now >= BIDDING_END,
                    (State::Performing, Action::Ready)
                    | (State::Performing, Action::KmReturnWinner)
                    | (State::Performing, Action::OperatorReturnWinner)
                    | (State::Performing, Action::ReturnEntry { in_winner_lot: true, .. }) => {
                        now < PERFORM_DEADLINE
                    }
                    (State::Voting { started_at }, Action::Vote(_))
                    | (State::Voting { started_at }, Action::OperatorReturnWinner)
                    | (
                        State::Voting { started_at },
                        Action::ReturnEntry { by: Actor::Operator, in_winner_lot: true },
                    ) => now < started_at + VOTING_PERIOD,
                    (_, Action::Tick) => true,
                    _ => false,
                };
                if result.is_ok() {
                    prop_assert!(drawn, "undrawn transition accepted: {:?}", action);
                } else {
                    // A failed action may still have advanced the clock,
                    // but never touches the vote record.
                    prop_assert_eq!(&a.votes, &before.votes);
                }
            }
        }

        // Verdict uniqueness: once Done, no sequence of actions changes
        // the state or the votes.
        #[test]
        fn done_is_absorbing(seq in actions(), road in 0u8..3) {
            let mut a = auction();
            match road {
                0 => step(&mut a, Action::KmCancel, T0 + 1).unwrap(),
                1 => step(&mut a, Action::Finale { winner: false }, BIDDING_END).unwrap(),
                _ => {
                    step(&mut a, Action::Finale { winner: true }, BIDDING_END).unwrap();
                    step(&mut a, Action::Ready, BIDDING_END + 1).unwrap();
                    step(&mut a, Action::Tick, BIDDING_END + 1 + VOTING_PERIOD).unwrap();
                }
            }
            let done = a.clone();
            let is_done = matches!(a.state, State::Done { .. });
            prop_assert!(is_done, "setup did not reach a verdict");
            for (action, now) in seq {
                let _ = step(&mut a, action, now);
                prop_assert_eq!(&a, &done);
            }
        }

        // Time: after bidding expiry no registry move ever lands again,
        // even if clocks jump back — FINALE_DUE is not BIDDING.
        #[test]
        fn expiry_is_irreversible(offsets in proptest::collection::vec(0u64..=DURATION * 3, 0..16)) {
            let mut a = auction();
            step(&mut a, Action::Tick, BIDDING_END).unwrap();
            for offset in offsets {
                let result = step(&mut a, register(), T0 + offset);
                prop_assert_eq!(result, Err(StepError::InvalidTransition));
                prop_assert!(!matches!(a.state, State::Bidding));
            }
        }

        // Determinism: one sequence of (action, now) gives one state.
        #[test]
        fn step_is_deterministic(seq in actions()) {
            let mut a = auction();
            let mut b = auction();
            for (action, now) in seq {
                let ra = step(&mut a, action.clone(), now);
                let rb = step(&mut b, action, now);
                prop_assert_eq!(ra, rb);
                prop_assert_eq!(&a, &b);
            }
        }

        // The tally at the end of voting equals the standalone verdict rule
        // over the recorded votes.
        #[test]
        fn tally_matches_verdict(seq in actions()) {
            let mut a = voting();
            let voting_end = BIDDING_END + 1 + VOTING_PERIOD;
            for (action, now) in seq {
                if let Action::Vote(_) = &action {
                    let _ = step(&mut a, action, now.min(voting_end - 1));
                }
            }
            let votes = a.votes.clone();
            step(&mut a, Action::Tick, voting_end).unwrap();
            prop_assert_eq!(
                &a.state,
                &State::Done { winner: Some(verdict(&votes)) }
            );
        }
    }

    #[test]
    fn logic_version_is_pinned() {
        assert_eq!(LOGIC_VERSION, 1);
    }
}
