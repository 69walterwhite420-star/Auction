//! The threshold key tree (docs/game-spec.md §2, §8): every lot gets its own
//! resolver — the Ed25519 pubkey of this canister's threshold key at
//! derivation path [lot_id] — and the on-demand verdict signing uses the
//! same path. The outcome is resolved from stable truth before any
//! signature is requested; a retry can only ever re-sign the same recorded
//! resolution.

use ic_cdk_management_canister::{
    SchnorrAlgorithm, SchnorrKeyId, SchnorrPublicKeyArgs, SignWithSchnorrArgs, schnorr_public_key,
    sign_with_schnorr,
};

fn schnorr_key_id() -> SchnorrKeyId {
    SchnorrKeyId {
        algorithm: SchnorrAlgorithm::Ed25519,
        name: crate::THRESHOLD_KEY.to_string(),
    }
}

/// The RESOLVER birth field of every escrow in one lot: the derived pubkey
/// at path [lot_id]. Stateless — fetched on demand for `get_resolver`,
/// stored in the lot record at first registration; the private key exists
/// nowhere.
pub(crate) async fn lot_resolver(lot_id: &[u8; 32]) -> Result<Vec<u8>, String> {
    let result = schnorr_public_key(&SchnorrPublicKeyArgs {
        canister_id: None,
        derivation_path: vec![lot_id.to_vec()],
        key_id: schnorr_key_id(),
    })
    .await
    .map_err(|error| format!("schnorr_public_key: {error}"))?;
    if result.public_key.len() != 32 {
        return Err("unexpected schnorr public key length".to_string());
    }
    Ok(result.public_key)
}

/// DOMAIN ‖ program ‖ escrow ‖ outcome — the ed25519_program message the
/// escrow demands right before claim (game-spec §12).
pub fn verdict_message(domain: &str, program: &[u8], escrow: &[u8], outcome: u8) -> Vec<u8> {
    let mut message = Vec::with_capacity(domain.len() + 65);
    message.extend_from_slice(domain.as_bytes());
    message.extend_from_slice(program);
    message.extend_from_slice(escrow);
    message.push(outcome);
    message
}

/// Signs a verdict message with the lot's derived key and sanity-checks the
/// result against the lot's resolver: a signature the chain would reject
/// must never leave the canister.
pub(crate) async fn sign_verdict(
    lot_id: &[u8],
    resolver: &[u8],
    message: &[u8],
) -> Result<Vec<u8>, String> {
    let result = sign_with_schnorr(&SignWithSchnorrArgs {
        message: message.to_vec(),
        derivation_path: vec![lot_id.to_vec()],
        key_id: schnorr_key_id(),
        aux: None,
    })
    .await
    .map_err(|error| format!("sign_with_schnorr: {error}"))?;
    let key: [u8; 32] = resolver
        .try_into()
        .map_err(|_| "stored resolver is not 32 bytes")?;
    let signature: [u8; 64] = result
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| "unexpected schnorr signature length")?;
    ed25519_dalek::VerifyingKey::from_bytes(&key)
        .and_then(|key| {
            key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(&signature))
        })
        .map_err(|_| "schnorr signature does not verify")?;
    Ok(signature.to_vec())
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

    // The verdict message mirrors the on-chain factory byte for byte:
    // DOMAIN ‖ program_id ‖ escrow ‖ outcome.
    #[test]
    fn verdict_message_layout_is_pinned() {
        let message = verdict_message("crown:two-outcome:solana-devnet", &[7; 32], &[9; 32], 1);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"crown:two-outcome:solana-devnet");
        expected.extend_from_slice(&[7; 32]);
        expected.extend_from_slice(&[9; 32]);
        expected.push(1);
        assert_eq!(message, expected);
    }
}
