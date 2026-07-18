//! Authorization is a wallet signature, never the calling principal
//! (docs/game-spec.md §6): the message layout the recipient and voters sign, its
//! verification, and the derivations that key auctions, name lot resolver
//! paths and rebuild escrow addresses.
//!
//! **Messages are UTF-8 text, and that is a hard requirement, not taste.**
//! Wallets refuse to sign bytes they cannot show to a human: Phantom runs
//! `isValidUTF8` over the payload and rejects everything else. A binary
//! protocol here means the game is unplayable with the largest Solana
//! wallet — and a signature nobody can read is a signature nobody should be
//! asked for.
//!
//! The text is a frozen protocol; the unit tests pin every line of it. The
//! auction_id and lot_id below stay binary on purpose: they are key
//! derivation inputs, never signed messages.

use sha2::{Digest, Sha256};

use crate::ChainSpec;

/// Domain separator of every participant message, and its first line.
/// Versioned: a canister with different rules is a different game and gets a
/// different domain.
pub const DOMAIN: &str = "crown:auction:v1";

/// Tag of the auction_id hash (game-spec §2). Unversioned: it is a key
/// derivation input, not a signature domain.
pub const AUCTION_TAG: &[u8] = b"crown:auction";

/// The vote, as the message spells it. Words are frozen forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Done,
    NotDone,
}

impl Choice {
    pub fn word(self) -> &'static str {
        match self {
            Choice::Done => "done",
            Choice::NotDone => "not_done",
        }
    }
}

/// What the participant is signing for. Carries the fields that belong to
/// that action and nothing else — an action and its payload cannot
/// disagree, because there is no separate payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Create {
        recipient_nonce: u64,
        duration: u64,
        perform_window: u64,
        min_entry: u64,
    },
    Accept {
        lot: [u8; 32],
    },
    ReturnLot {
        lot: [u8; 32],
    },
    ReturnEntry {
        escrow: Vec<u8>,
    },
    Cancel,
    Ready,
    Vote(Choice),
    OperatorRefundLot {
        lot: [u8; 32],
    },
    OperatorRefundEntry {
        escrow: Vec<u8>,
    },
    OperatorCancel,
}

impl Action {
    /// The word that names the action in the message. Frozen forever.
    pub fn word(&self) -> &'static str {
        match self {
            Action::Create { .. } => "create",
            Action::Accept { .. } => "accept",
            Action::ReturnLot { .. } => "return-lot",
            Action::ReturnEntry { .. } => "return-entry",
            Action::Cancel => "cancel",
            Action::Ready => "ready",
            Action::Vote(_) => "vote",
            Action::OperatorRefundLot { .. } => "operator-refund-lot",
            Action::OperatorRefundEntry { .. } => "operator-refund-entry",
            Action::OperatorCancel => "operator-cancel",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    UnknownChain,
    BadFieldLength,
    BadSignature,
    MalformedConfig,
    DeadlineOverflow,
    NoAddress,
}

impl AuthError {
    pub fn text(self) -> &'static str {
        match self {
            AuthError::UnknownChain => "unknown chain",
            AuthError::BadFieldLength => "bad field length",
            AuthError::BadSignature => "bad signature",
            AuthError::MalformedConfig => "malformed chain config",
            AuthError::DeadlineOverflow => "deadline does not fit the chain",
            AuthError::NoAddress => "escrow address does not exist",
        }
    }
}

pub fn spec_of(chain: &str) -> Result<&'static ChainSpec, AuthError> {
    crate::CHAINS
        .iter()
        .find(|spec| spec.id == chain)
        .ok_or(AuthError::UnknownChain)
}

/// The message a participant signs about one auction. One field per line,
/// `key: value`, in this exact order:
///
/// ```text
/// crown:auction:v1
/// action: accept
/// chain: solana-devnet
/// canister: vpyes-67777-77774-qaaeq-cai
/// auction: 166b43c4ed39cd43693e547bb52ce1c60acce8db5786b6e4d56547e67f018f47
/// lot: e2d80f78d79027556d6619a1400605abbdca6bb6eb24e0831e33ecd5466fa5f6
/// ```
///
/// The first five lines — domain, `action:`, `chain:`, `canister:`,
/// `auction:` — open every action except `create`, which has no `auction:`
/// line (the id derives from its `recipient_nonce:`). `accept`, `return-lot` and
/// `operator-refund-lot` add `lot:` (hex); `return-entry` and
/// `operator-refund-entry` add `escrow:` (base58); `vote` adds `choice:`;
/// `cancel`, `ready` and `operator-cancel` add nothing.
///
/// The encoding is injective — two different messages cannot render to the
/// same text — because the keys are fixed and ordered, the action decides
/// which keys follow, and no value can contain a newline: ids are hex,
/// addresses base58, numbers decimal, words a closed vocabulary, and
/// `validate_config` refuses a chain id with anything else in it.
pub fn auction_message(
    chain: &str,
    canister_id: &str,
    auction_id: &[u8],
    action: &Action,
) -> String {
    let mut out = String::new();
    out.push_str(DOMAIN);
    out.push('\n');
    out.push_str(&format!("action: {}\n", action.word()));
    out.push_str(&format!("chain: {chain}\n"));
    out.push_str(&format!("canister: {canister_id}\n"));
    if !matches!(action, Action::Create { .. }) {
        // The auction id is an opaque hash, not an address: hex is its form.
        out.push_str(&format!("auction: {}\n", hex(auction_id)));
    }
    match action {
        Action::Create {
            recipient_nonce,
            duration,
            perform_window,
            min_entry,
        } => {
            out.push_str(&format!("recipient_nonce: {recipient_nonce}\n"));
            out.push_str(&format!("duration: {duration}\n"));
            out.push_str(&format!("perform_window: {perform_window}\n"));
            out.push_str(&format!("min_entry: {min_entry}\n"));
        }
        Action::Accept { lot } | Action::ReturnLot { lot } | Action::OperatorRefundLot { lot } => {
            out.push_str(&format!("lot: {}\n", hex(lot)));
        }
        Action::ReturnEntry { escrow } | Action::OperatorRefundEntry { escrow } => {
            out.push_str(&format!("escrow: {}\n", bs58::encode(escrow).into_string()));
        }
        Action::Vote(choice) => out.push_str(&format!("choice: {}\n", choice.word())),
        Action::Cancel | Action::Ready | Action::OperatorCancel => {}
    }
    out
}

/// Lowercase hex. `hex` is a dev-dependency only, and one line of code is
/// cheaper than making it a runtime one.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// auction_id = sha256(TAG ‖ len(canister_id) u8 ‖ canister_id ‖ recipient ‖
/// recipient_nonce_le) — deterministic, published in the auction's link, the key of
/// the auction and the first half of every lot_id (game-spec §2). The
/// principal is length-prefixed: principals vary in length, so the encoding
/// must be injective.
pub fn derive_auction_id(
    canister_id: &[u8],
    recipient: &[u8],
    recipient_nonce: u64,
) -> Result<[u8; 32], AuthError> {
    let recipient: [u8; 32] = recipient
        .try_into()
        .map_err(|_| AuthError::BadFieldLength)?;
    let len: u8 = canister_id
        .len()
        .try_into()
        .map_err(|_| AuthError::BadFieldLength)?;
    let mut hasher = Sha256::new();
    hasher.update(AUCTION_TAG);
    hasher.update([len]);
    hasher.update(canister_id);
    hasher.update(recipient);
    hasher.update(recipient_nonce.to_le_bytes());
    Ok(hasher.finalize().into())
}

/// lot_id = sha256(auction_id ‖ text_hash): both halves are fixed 32-byte
/// hashes, so the concatenation is injective without prefixes. The lot_id
/// is the derivation path of the lot's resolver — money, text and auction
/// are welded together by this hash (game-spec §2).
pub fn derive_lot_id(auction_id: &[u8], text_hash: &[u8]) -> Result<[u8; 32], AuthError> {
    let auction_id: [u8; 32] = auction_id
        .try_into()
        .map_err(|_| AuthError::BadFieldLength)?;
    let text_hash: [u8; 32] = text_hash
        .try_into()
        .map_err(|_| AuthError::BadFieldLength)?;
    let mut hasher = Sha256::new();
    hasher.update(auction_id);
    hasher.update(text_hash);
    Ok(hasher.finalize().into())
}

/// The escrow address of one entry, derived from the declared birth fields
/// with the same arithmetic the core's indexer uses (game-spec §8,
/// factory-spec §4). `recipient` and `resolver` come from the auction record and
/// the lot derivation, never from the requester. Returns the address and
/// the salt — registration compares the salt against the on-chain account
/// (game-spec §4).
pub fn derive_escrow(
    spec: &ChainSpec,
    donor: &[u8],
    recipient: &[u8],
    gross: u64,
    deadline: u64,
    resolver: &[u8],
    nonce: u64,
) -> Result<(Vec<u8>, [u8; 32]), AuthError> {
    let donor: [u8; 32] = donor.try_into().map_err(|_| AuthError::BadFieldLength)?;
    let recipient: [u8; 32] = recipient
        .try_into()
        .map_err(|_| AuthError::BadFieldLength)?;
    let resolver: [u8; 32] = resolver.try_into().map_err(|_| AuthError::BadFieldLength)?;
    // The on-chain program takes deadline as i64; out-of-range is caught here.
    let deadline = i64::try_from(deadline).map_err(|_| AuthError::DeadlineOverflow)?;
    // The game's fee is part of the salt: an escrow born with a price other
    // than this game's derives a different address and never joins a lot.
    let fee_wallet: [u8; 32] = bs58::decode(spec.fee_wallet)
        .into_vec()
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(AuthError::MalformedConfig)?;
    // The shape owns its byte format: `crown-salt` is the single offchain
    // definition of the salt, parity-tested against the deployed program's
    // `birth_salt`. The recipient is the recipient of every escrow in the auction.
    let salt = crown_salt::two_outcome::salt(
        &donor,
        &recipient,
        gross,
        deadline,
        &resolver,
        spec.fee_bps,
        &fee_wallet,
        nonce,
    );

    let program: [u8; 32] = bs58::decode(spec.factory)
        .into_vec()
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(AuthError::MalformedConfig)?;
    let (address, _bump) = crown_derive::solana_pda_address(program, &[b"escrow", &salt])
        .ok_or(AuthError::NoAddress)?;
    Ok((address.to_vec(), salt))
}

/// Verifies a wallet signature over `message` by `signer` — the wallet's
/// address bytes. Wallets sign the raw message with Ed25519 (64 bytes),
/// the address being the public key itself.
pub fn verify_wallet_signature(
    message: &[u8],
    signature: &[u8],
    signer: &[u8],
) -> Result<(), AuthError> {
    let signer: [u8; 32] = signer.try_into().map_err(|_| AuthError::BadFieldLength)?;
    let signature: [u8; 64] = signature.try_into().map_err(|_| AuthError::BadSignature)?;
    let key =
        ed25519_dalek::VerifyingKey::from_bytes(&signer).map_err(|_| AuthError::BadSignature)?;
    key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(&signature))
        .map_err(|_| AuthError::BadSignature)
}

/// Deploy-time validation: every baked chain entry must parse — including
/// the read side, `source` and `consensus` (game-spec §4) — and a mainnet
/// build must never name a Custom source. A canister with a malformed
/// config must not exist.
pub fn validate_config() -> Result<(), AuthError> {
    // The operator wallet is empty until a real deploy pins it; non-empty it
    // must be a valid address.
    if !crate::OPERATOR_WALLET.is_empty() {
        bs58::decode(crate::OPERATOR_WALLET)
            .into_vec()
            .ok()
            .filter(|b| b.len() == 32)
            .ok_or(AuthError::MalformedConfig)?;
    }
    for (i, spec) in crate::CHAINS.iter().enumerate() {
        bs58::decode(spec.factory)
            .into_vec()
            .ok()
            .filter(|b| b.len() == 32)
            .ok_or(AuthError::MalformedConfig)?;
        bs58::decode(spec.fee_wallet)
            .into_vec()
            .ok()
            .filter(|b| b.len() == 32)
            .ok_or(AuthError::MalformedConfig)?;
        if spec.fee_bps >= 10_000 {
            return Err(AuthError::MalformedConfig);
        }
        if spec.domain.is_empty() {
            return Err(AuthError::MalformedConfig);
        }
        // The read side must parse, and the mainnet profile must never name
        // its own sources (core-spec §8) — the config lint enforces the
        // same rule at build time, this refuses a bypassed build to run.
        crate::solana::parse_sources(spec.source).ok_or(AuthError::MalformedConfig)?;
        crate::solana::parse_consensus(spec.consensus).ok_or(AuthError::MalformedConfig)?;
        if crate::PROFILE == "mainnet" && spec.source.starts_with("Custom") {
            return Err(AuthError::MalformedConfig);
        }
        // The chain id goes into the signed text as a value. A newline (or a
        // control character) in it would let one chain id render a message
        // another chain id could also render — the encoding must stay
        // injective, so refuse such a config to exist at all.
        if spec.id.is_empty()
            || !spec
                .id
                .chars()
                .all(|c| c.is_ascii_graphic() && c != ':' && c != '\n')
        {
            return Err(AuthError::MalformedConfig);
        }
        // Chains must be pairwise distinct in id, domain and factory. The
        // auction_id and lot_id are chain-independent, so one lot resolver
        // serves the same (auction, text) on every chain; the cluster is
        // separated only by DOMAIN and the factory in the salt (factory-spec
        // §2.2). Two chain entries sharing a (factory, domain) would derive
        // one escrow address for the same birth fields under two chain keys,
        // and the registry could then hold two verdicts for one escrow.
        // Refuse such a config to exist.
        for other in crate::CHAINS.iter().skip(i + 1) {
            if spec.id == other.id || spec.domain == other.domain || spec.factory == other.factory {
                return Err(AuthError::MalformedConfig);
            }
        }
    }
    // The baked book principal, when present, must parse. An empty value is
    // legal: the init override supplies it on local replicas (G3).
    if !crate::CROWN_INDEX.is_empty() && candid::Principal::from_text(crate::CROWN_INDEX).is_err() {
        return Err(AuthError::MalformedConfig);
    }
    Ok(())
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
    use super::*;

    // ---- frozen message layouts -----------------------------------------

    const CANISTER: &str = "vpyes-67777-77774-qaaeq-cai";
    const AUCTION: [u8; 32] = [0xAA; 32];
    const AUCTION_HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LOT: [u8; 32] = [0xBB; 32];
    const LOT_HEX: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn create_message_is_pinned() {
        assert_eq!(
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Create {
                    recipient_nonce: 7,
                    duration: 86_400,
                    perform_window: 43_200,
                    min_entry: 50,
                },
            ),
            format!(
                "crown:auction:v1\n\
                 action: create\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 recipient_nonce: 7\n\
                 duration: 86400\n\
                 perform_window: 43200\n\
                 min_entry: 50\n"
            )
        );
    }

    #[test]
    fn accept_message_is_pinned() {
        assert_eq!(
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Accept { lot: LOT },
            ),
            format!(
                "crown:auction:v1\n\
                 action: accept\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n\
                 lot: {LOT_HEX}\n"
            )
        );
    }

    #[test]
    fn return_lot_message_is_pinned() {
        assert_eq!(
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::ReturnLot { lot: LOT },
            ),
            format!(
                "crown:auction:v1\n\
                 action: return-lot\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n\
                 lot: {LOT_HEX}\n"
            )
        );
    }

    #[test]
    fn return_entry_message_is_pinned() {
        // base58 of [0x11; 32], the escrow address as explorers show it.
        let escrow = vec![0x11; 32];
        assert_eq!(
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::ReturnEntry {
                    escrow: escrow.clone()
                },
            ),
            format!(
                "crown:auction:v1\n\
                 action: return-entry\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n\
                 escrow: {}\n",
                bs58::encode(&escrow).into_string()
            )
        );
    }

    #[test]
    fn cancel_message_is_pinned() {
        assert_eq!(
            auction_message("solana-devnet", CANISTER, &AUCTION, &Action::Cancel),
            format!(
                "crown:auction:v1\n\
                 action: cancel\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n"
            )
        );
    }

    #[test]
    fn ready_message_is_pinned() {
        assert_eq!(
            auction_message("solana-devnet", CANISTER, &AUCTION, &Action::Ready),
            format!(
                "crown:auction:v1\n\
                 action: ready\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n"
            )
        );
    }

    #[test]
    fn vote_message_is_pinned() {
        for (choice, word) in [(Choice::Done, "done"), (Choice::NotDone, "not_done")] {
            assert_eq!(
                auction_message("solana-devnet", CANISTER, &AUCTION, &Action::Vote(choice)),
                format!(
                    "crown:auction:v1\n\
                     action: vote\n\
                     chain: solana-devnet\n\
                     canister: {CANISTER}\n\
                     auction: {AUCTION_HEX}\n\
                     choice: {word}\n"
                )
            );
        }
    }

    #[test]
    fn operator_refund_messages_are_pinned() {
        assert_eq!(
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::OperatorRefundLot { lot: LOT },
            ),
            format!(
                "crown:auction:v1\n\
                 action: operator-refund-lot\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n\
                 lot: {LOT_HEX}\n"
            )
        );
        let escrow = vec![0x11; 32];
        assert_eq!(
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::OperatorRefundEntry {
                    escrow: escrow.clone()
                },
            ),
            format!(
                "crown:auction:v1\n\
                 action: operator-refund-entry\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n\
                 escrow: {}\n",
                bs58::encode(&escrow).into_string()
            )
        );
        assert_eq!(
            auction_message("solana-devnet", CANISTER, &AUCTION, &Action::OperatorCancel),
            format!(
                "crown:auction:v1\n\
                 action: operator-cancel\n\
                 chain: solana-devnet\n\
                 canister: {CANISTER}\n\
                 auction: {AUCTION_HEX}\n"
            )
        );
    }

    /// The whole point: a wallet must be able to show this to a human.
    /// Phantom rejects anything that is not valid UTF-8.
    #[test]
    fn every_message_is_printable_ascii() {
        let messages = [
            auction_message(
                "solana-devnet",
                CANISTER,
                &[0xFF; 32],
                &Action::Create {
                    recipient_nonce: u64::MAX,
                    duration: u64::MAX,
                    perform_window: u64::MAX,
                    min_entry: u64::MAX,
                },
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Accept { lot: [0xFF; 32] },
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::ReturnEntry {
                    escrow: vec![0xFF; 32],
                },
            ),
            auction_message("solana-devnet", CANISTER, &AUCTION, &Action::Cancel),
        ];
        for message in messages {
            assert!(
                message
                    .chars()
                    .all(|c| c == '\n' || c.is_ascii_graphic() || c == ' '),
                "not printable: {message:?}"
            );
        }
    }

    /// Injectivity: no two distinct messages may render the same text.
    #[test]
    fn distinct_messages_render_distinctly() {
        let messages = [
            auction_message("solana-devnet", CANISTER, &AUCTION, &Action::Cancel),
            auction_message("solana-devnet", CANISTER, &[0xAC; 32], &Action::Cancel),
            auction_message("solana-mainnet", CANISTER, &AUCTION, &Action::Cancel),
            auction_message("solana-devnet", "aaaaa-aa", &AUCTION, &Action::Cancel),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Accept { lot: LOT },
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::ReturnLot { lot: LOT },
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::ReturnEntry {
                    escrow: vec![0x11; 32],
                },
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Create {
                    recipient_nonce: 1,
                    duration: 60,
                    perform_window: 60,
                    min_entry: 0,
                },
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Create {
                    recipient_nonce: 2,
                    duration: 60,
                    perform_window: 60,
                    min_entry: 0,
                },
            ),
            auction_message("solana-devnet", CANISTER, &AUCTION, &Action::Ready),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Vote(Choice::Done),
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::Vote(Choice::NotDone),
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::OperatorRefundLot { lot: LOT },
            ),
            auction_message(
                "solana-devnet",
                CANISTER,
                &AUCTION,
                &Action::OperatorRefundEntry {
                    escrow: vec![0x11; 32],
                },
            ),
            auction_message("solana-devnet", CANISTER, &AUCTION, &Action::OperatorCancel),
        ];
        let count = messages.len();
        let seen: std::collections::BTreeSet<String> = messages.into_iter().collect();
        assert_eq!(seen.len(), count, "две разные декларации дали один текст");
    }

    // ---- id derivations ---------------------------------------------------

    /// Cross-tool vector, computed independently with python hashlib:
    /// sha256(b"crown:auction" ‖ [10] ‖ [0x01]×10 ‖ [0x22]×32 ‖ le64(7)).
    const AUCTION_ID_VECTOR: &str =
        "166b43c4ed39cd43693e547bb52ce1c60acce8db5786b6e4d56547e67f018f47";

    /// Cross-tool vector: sha256([0xAA]×32 ‖ [0xBB]×32).
    const LOT_ID_VECTOR: &str = "e2d80f78d79027556d6619a1400605abbdca6bb6eb24e0831e33ecd5466fa5f6";

    fn unhex(s: &str) -> Vec<u8> {
        s.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// The id derivation feeds every lot's resolver path: if these bytes
    /// move, every lot's resolver moves with them.
    #[test]
    fn auction_id_matches_reference_vector() {
        let id = derive_auction_id(&[0x01; 10], &[0x22; 32], 7).unwrap();
        assert_eq!(id.to_vec(), unhex(AUCTION_ID_VECTOR));
    }

    #[test]
    fn lot_id_matches_reference_vector() {
        let id = derive_lot_id(&[0xAA; 32], &[0xBB; 32]).unwrap();
        assert_eq!(id.to_vec(), unhex(LOT_ID_VECTOR));
    }

    #[test]
    fn id_derivations_reject_wrong_lengths() {
        assert_eq!(
            derive_auction_id(&[0x01; 10], &[0x22; 31], 7),
            Err(AuthError::BadFieldLength)
        );
        assert_eq!(
            derive_lot_id(&[0xAA; 31], &[0xBB; 32]),
            Err(AuthError::BadFieldLength)
        );
        assert_eq!(
            derive_lot_id(&[0xAA; 32], &[0xBB; 33]),
            Err(AuthError::BadFieldLength)
        );
    }

    #[test]
    fn id_derivations_separate_every_input() {
        let base = derive_auction_id(&[0x01; 10], &[0x22; 32], 7).unwrap();
        assert_ne!(
            base,
            derive_auction_id(&[0x02; 10], &[0x22; 32], 7).unwrap()
        );
        assert_ne!(
            base,
            derive_auction_id(&[0x01; 10], &[0x23; 32], 7).unwrap()
        );
        assert_ne!(
            base,
            derive_auction_id(&[0x01; 10], &[0x22; 32], 8).unwrap()
        );
        let lot = derive_lot_id(&[0xAA; 32], &[0xBB; 32]).unwrap();
        assert_ne!(lot, derive_lot_id(&[0xAA; 32], &[0xBC; 32]).unwrap());
        assert_ne!(lot, derive_lot_id(&[0xAB; 32], &[0xBB; 32]).unwrap());
    }

    // ---- signatures -------------------------------------------------------

    #[test]
    fn signature_roundtrip_and_rejections() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let address = key.verifying_key().to_bytes().to_vec();
        let message = auction_message("solana-devnet", CANISTER, &AUCTION, &Action::Cancel);
        let sig = key.sign(message.as_bytes()).to_bytes().to_vec();
        verify_wallet_signature(message.as_bytes(), &sig, &address).unwrap();

        // Foreign signer.
        let other = ed25519_dalek::SigningKey::from_bytes(&[10; 32])
            .verifying_key()
            .to_bytes()
            .to_vec();
        assert_eq!(
            verify_wallet_signature(message.as_bytes(), &sig, &other),
            Err(AuthError::BadSignature)
        );
        // Foreign auction: same signer, different id.
        let foreign = auction_message("solana-devnet", CANISTER, &[0xAC; 32], &Action::Cancel);
        assert_eq!(
            verify_wallet_signature(foreign.as_bytes(), &sig, &address),
            Err(AuthError::BadSignature)
        );
        // Foreign canister.
        let foreign = auction_message("solana-devnet", "aaaaa-aa", &AUCTION, &Action::Cancel);
        assert_eq!(
            verify_wallet_signature(foreign.as_bytes(), &sig, &address),
            Err(AuthError::BadSignature)
        );
        // Foreign action: a cancel signature does not accept a lot.
        let foreign = auction_message(
            "solana-devnet",
            CANISTER,
            &AUCTION,
            &Action::Accept { lot: LOT },
        );
        assert_eq!(
            verify_wallet_signature(foreign.as_bytes(), &sig, &address),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn baked_config_is_valid() {
        validate_config().unwrap();
    }

    // ---- escrow derivation ------------------------------------------------

    fn spec() -> ChainSpec {
        ChainSpec {
            id: "solana-devnet",
            factory: "83f7ziVs5VeQ8xiDka8zczbfJT4WcxsXQ18cqWwmV5ur",
            domain: "crown:two-outcome:solana-devnet",
            source: "Default:Devnet",
            consensus: "3-of-5",
            fee_bps: 500,
            // base58 of [0x44; 32], matching the crown-salt reference vector.
            fee_wallet: "5bV6jUfhDHCQVA1WfKBUnXUsboJgoKgkzkKcxr3joew5",
        }
    }

    // Frozen cross-tool vector: salt is sha256 over the exact byte concat,
    // computed independently with python3 hashlib over donor ‖ recipient ‖
    // u64le(1000000) ‖ i64le(1900000000) ‖ resolver ‖ u16le(500) ‖ fee_wallet
    // ‖ u64le(7); the PDA arithmetic itself is parity-tested in crown-derive.
    const SALT_VECTOR: &str = "149c82b09a080ef4c92921d13d974177bfea2dd546ef8b798627e3e4245afe6b";

    #[test]
    fn escrow_matches_reference_salt() {
        let donor = [0x11; 32];
        let recipient = [0x22; 32];
        let resolver = [0x33; 32];
        let (escrow, salt) = derive_escrow(
            &spec(),
            &donor,
            &recipient,
            1_000_000,
            1_900_000_000,
            &resolver,
            7,
        )
        .unwrap();
        assert_eq!(salt.to_vec(), unhex(SALT_VECTOR));

        let program: [u8; 32] = bs58::decode(spec().factory)
            .into_vec()
            .unwrap()
            .try_into()
            .unwrap();
        let (expected, _) = crown_derive::solana_pda_address(program, &[b"escrow", &salt]).unwrap();
        assert_eq!(escrow, expected.to_vec());
    }

    #[test]
    fn escrow_derivation_rejects_bad_inputs() {
        assert_eq!(
            derive_escrow(&spec(), &[0x11; 31], &[0x22; 32], 34, 1, &[0x33; 32], 0),
            Err(AuthError::BadFieldLength)
        );
        assert_eq!(
            derive_escrow(
                &spec(),
                &[0x11; 32],
                &[0x22; 32],
                34,
                u64::MAX,
                &[0x33; 32],
                0
            ),
            Err(AuthError::DeadlineOverflow)
        );
    }
}
