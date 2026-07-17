//! The finale law: the richest accepted lot wins; ties go to the standing
//! assembled earliest (docs/game-spec.md §9).
//!
//! The caller folds each accepted lot's entries into a `Standing` with
//! `standing`, then picks the winner with `winner` — or folds chunk by
//! chunk with `beats`, which is the same comparison. Returned entries are
//! dead: they leave both the sum and the seq resolution.

/// One registered entry of a lot, as the caller's registry remembers it:
/// the escrow's gross, the monotonic registration seq, and whether the
/// entry still lives (was not returned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub gross: u64,
    pub seq: u64,
    pub alive: bool,
}

/// One lot's weight in the finale: the sum of its live grosses and the seq
/// of the last live registration — the moment its present composition
/// finished assembling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    pub sum: u128,
    pub last_seq: u64,
}

/// Folds a lot's entries into its standing. `None` when nothing stands:
/// an empty lot, a fully returned lot, or a zero sum — none of them can
/// win (game-spec §9). The u128 sum of u64 grosses cannot overflow for any
/// physically possible registry; the checked add makes the claim a proof.
pub fn standing(entries: &[Entry]) -> Option<Standing> {
    let mut sum: u128 = 0;
    let mut last_seq: Option<u64> = None;
    for entry in entries.iter().filter(|e| e.alive) {
        sum = sum.checked_add(u128::from(entry.gross))?;
        last_seq = Some(last_seq.map_or(entry.seq, |s| s.max(entry.seq)));
    }
    if sum == 0 {
        return None;
    }
    last_seq.map(|last_seq| Standing { sum, last_seq })
}

/// True when `challenger` beats `incumbent`: strictly richer, or equally
/// rich but assembled earlier (lower seq of the last live registration).
/// Seqs are unique across a registry by construction, so equal sums with
/// equal seqs cannot arise within one auction; a fold keeps the incumbent
/// then, staying deterministic on any input.
pub fn beats(challenger: &Standing, incumbent: &Standing) -> bool {
    challenger.sum > incumbent.sum
        || (challenger.sum == incumbent.sum && challenger.last_seq < incumbent.last_seq)
}

/// The winner among the accepted lots' standings (as built by `standing`):
/// a `beats`-fold; the first index wins exact stalemates. Returns the index
/// into `standings`.
pub fn winner(standings: &[Standing]) -> Option<usize> {
    let mut best: Option<(usize, &Standing)> = None;
    for (i, s) in standings.iter().enumerate() {
        let replace = match best {
            None => true,
            Some((_, incumbent)) => beats(s, incumbent),
        };
        if replace {
            best = Some((i, s));
        }
    }
    best.map(|(i, _)| i)
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

    fn entry(gross: u64, seq: u64, alive: bool) -> Entry {
        Entry { gross, seq, alive }
    }

    // --- standing -------------------------------------------------------

    #[test]
    fn standing_sums_live_entries_only() {
        let entries = [entry(150, 1, true), entry(50, 5, true)];
        assert_eq!(
            standing(&entries),
            Some(Standing {
                sum: 200,
                last_seq: 5
            })
        );
    }

    #[test]
    fn returned_entry_leaves_sum_and_seq() {
        // Killing the seq-5 top-up rolls the standing back to the moment
        // the remaining composition assembled: seq 1.
        let entries = [entry(150, 1, true), entry(50, 5, false)];
        assert_eq!(
            standing(&entries),
            Some(Standing {
                sum: 150,
                last_seq: 1
            })
        );
    }

    #[test]
    fn nothing_stands_without_live_entries() {
        assert_eq!(standing(&[]), None);
        assert_eq!(standing(&[entry(100, 1, false), entry(7, 2, false)]), None);
    }

    #[test]
    fn zero_sum_never_stands() {
        assert_eq!(standing(&[entry(0, 1, true)]), None);
    }

    proptest! {
        // The standing is exactly the recount over live entries.
        #[test]
        fn standing_equals_recount(
            entries in proptest::collection::vec(
                (0u64..=u64::MAX, any::<bool>()),
                0..24,
            )
        ) {
            // Unique seqs by construction, as in a real registry.
            let entries: Vec<Entry> = entries
                .into_iter()
                .enumerate()
                .map(|(i, (gross, alive))| entry(gross, i as u64, alive))
                .collect();
            let sum: u128 = entries
                .iter()
                .filter(|e| e.alive)
                .map(|e| u128::from(e.gross))
                .sum();
            let last_seq = entries.iter().filter(|e| e.alive).map(|e| e.seq).max();
            let expected = match (sum, last_seq) {
                (0, _) | (_, None) => None,
                (sum, Some(last_seq)) => Some(Standing { sum, last_seq }),
            };
            prop_assert_eq!(standing(&entries), expected);
        }
    }

    // --- winner -----------------------------------------------------------

    #[test]
    fn richest_lot_wins() {
        let standings = [
            Standing {
                sum: 100,
                last_seq: 1,
            },
            Standing {
                sum: 300,
                last_seq: 9,
            },
            Standing {
                sum: 200,
                last_seq: 2,
            },
        ];
        assert_eq!(winner(&standings), Some(1));
    }

    #[test]
    fn tie_goes_to_the_earliest_assembled() {
        // Lot 1 reached the same sum earlier (lower last live seq): it wins
        // even though lot 0 registered its first entry first.
        let standings = [
            Standing {
                sum: 300,
                last_seq: 8,
            },
            Standing {
                sum: 300,
                last_seq: 3,
            },
        ];
        assert_eq!(winner(&standings), Some(1));
    }

    #[test]
    fn no_standings_no_winner() {
        assert_eq!(winner(&[]), None);
    }

    /// Standings with unique seqs, as `standing` over one registry yields.
    fn standings() -> impl Strategy<Value = Vec<Standing>> {
        proptest::collection::vec(1u128..=u128::from(u64::MAX), 0..24).prop_map(|sums| {
            sums.into_iter()
                .enumerate()
                .map(|(i, sum)| Standing {
                    sum,
                    last_seq: i as u64,
                })
                .collect()
        })
    }

    proptest! {
        // The fold equals the brute-force argmax by (sum desc, last_seq asc).
        #[test]
        fn winner_equals_brute_force(ss in standings()) {
            let expected = ss
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| {
                    a.sum.cmp(&b.sum).then(b.last_seq.cmp(&a.last_seq))
                })
                .map(|(i, _)| i);
            prop_assert_eq!(winner(&ss), expected);
        }

        // Permutations pick the same standing, not the same index: the law
        // is about lots, not about the order the caller happened to scan.
        #[test]
        fn winner_is_permutation_invariant(ss in standings()) {
            let mut reversed: Vec<Standing> = ss.clone();
            reversed.reverse();
            let by_content = |v: &[Standing], i: Option<usize>| i.map(|i| v[i]);
            prop_assert_eq!(
                by_content(&ss, winner(&ss)),
                by_content(&reversed, winner(&reversed))
            );
        }

        // beats is a strict total order on standings with unique seqs:
        // exactly one direction holds for two distinct standings.
        #[test]
        fn beats_is_a_strict_order(ss in standings()) {
            for a in &ss {
                prop_assert!(!beats(a, a));
                for b in &ss {
                    if a != b {
                        prop_assert!(beats(a, b) != beats(b, a));
                    }
                }
            }
        }
    }
}
