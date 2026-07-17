//! The threshold key tree (docs/game-spec.md §2, §8): every lot gets its own
//! resolver — the Ed25519 pubkey of this canister's threshold key at
//! derivation path [lot_id]. G3 adds the on-demand verdict signing over the
//! same path; verdicts live in stable memory before any signature is
//! requested.

use ic_cdk_management_canister::{
    SchnorrAlgorithm, SchnorrKeyId, SchnorrPublicKeyArgs, schnorr_public_key,
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
