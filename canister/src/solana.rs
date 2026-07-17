//! The single read of the outside world (docs/game-spec.md §4): one
//! `getAccountInfo` per registration, through the SOL RPC canister under
//! NNS, `commitment = finalized`, provider consensus per config. The client
//! pattern mirrors the core's `index/src/source/solana.rs`.
//!
//! The read gates only the leaderboard: a declared entry becomes a bid when
//! the finalized chain confirms the account exists, is owned by the pinned
//! factory and carries exactly the declared birth (donor and salt at the
//! header convention offsets, factory-spec §2.1). A transport error is a
//! call error, never a write and never a guess.

use ic_canister_runtime::IcRuntime;
use sol_rpc_client::SolRpcClient;
use sol_rpc_types::{
    CommitmentLevel, ConsensusStrategy, GetAccountInfoEncoding, MultiRpcResult, RpcConfig,
    RpcEndpoint, RpcSource, RpcSources, SolanaCluster,
};
use solana_pubkey::Pubkey;

use crate::ChainSpec;

/// sha256("account:Escrow")[..8] — identical for every shape by the header
/// convention (factory-spec §2.1); the same constant the core's parser pins.
const ESCROW_ACCOUNT_DISCRIMINATOR: [u8; 8] = [31, 213, 123, 187, 186, 22, 218, 155];

/// Byte offset of `settled` in the two-outcome Escrow account: discriminator
/// 8 ‖ donor 32 ‖ salt 32 ‖ streamer 32 ‖ resolver 32 ‖ gross 8 ‖ deadline 8
/// ‖ fee_bps 2 ‖ fee_wallet 32 ‖ bump 1 — then settled. Shape-specific: this
/// is a two-outcome game (game-spec §12); the layout is pinned by the g2
/// registration fixture.
const SETTLED_OFFSET: usize = 187;

/// "Default:Mainnet" | "Default:Devnet" | "Custom:<url>" — the same grammar
/// the core's config speaks. Custom is testnet-only (validate_config).
pub(crate) fn parse_sources(source: &str) -> Option<RpcSources> {
    match source.split_once(':')? {
        ("Default", "Mainnet") => Some(RpcSources::Default(SolanaCluster::Mainnet)),
        ("Default", "Devnet") => Some(RpcSources::Default(SolanaCluster::Devnet)),
        ("Custom", url) => Some(RpcSources::Custom(vec![RpcSource::Custom(RpcEndpoint {
            url: url.to_string(),
            headers: None,
        })])),
        _ => None,
    }
}

/// "equality" | "<min>-of-<total>".
pub(crate) fn parse_consensus(consensus: &str) -> Option<ConsensusStrategy> {
    if consensus == "equality" {
        return Some(ConsensusStrategy::Equality);
    }
    let (min, total) = consensus.split_once("-of-")?;
    Some(ConsensusStrategy::Threshold {
        min: min.parse().ok()?,
        total: Some(total.parse().ok()?),
    })
}

fn client(spec: &ChainSpec) -> Result<SolRpcClient<IcRuntime>, String> {
    let sources = parse_sources(spec.source).ok_or("malformed rpc source in config")?;
    let consensus = parse_consensus(spec.consensus).ok_or("malformed rpc consensus in config")?;
    Ok(
        SolRpcClient::builder(IcRuntime::new(), crate::sol_rpc_canister())
            .with_rpc_sources(sources)
            .with_rpc_config(RpcConfig {
                response_consensus: Some(consensus),
                ..Default::default()
            })
            .with_default_commitment_level(CommitmentLevel::Finalized)
            .build(),
    )
}

/// Verifies one declared entry against the finalized chain (game-spec §4):
/// the account at the derived address exists, is owned by the factory,
/// carries the escrow discriminator, the declared donor and the recomputed
/// salt at the convention offsets, and is not settled. Existence at the
/// derived address proves the escrow was born by the factory with exactly
/// these fields and funded in full — an underfunded escrow does not exist
/// (factory-spec §2.1).
pub(crate) async fn verify_escrow(
    spec: &ChainSpec,
    escrow: &[u8],
    donor: &[u8],
    salt: &[u8; 32],
) -> Result<(), String> {
    let pubkey = Pubkey::try_from(escrow).map_err(|_| "escrow address is not a pubkey")?;
    let request = client(spec)?
        .get_account_info(pubkey)
        .with_encoding(GetAccountInfoEncoding::Base64);
    // Cycles price depends on request and provider set: ask, then attach.
    let cost = request
        .clone()
        .request_cost()
        .send()
        .await
        .map_err(|e| format!("sol-rpc: getAccountInfo cost: {e}"))?;
    let account = match request.with_cycles(cost).send().await {
        MultiRpcResult::Consistent(Ok(account)) => account,
        MultiRpcResult::Consistent(Err(e)) => {
            return Err(format!("sol-rpc: getAccountInfo: {e}"));
        }
        MultiRpcResult::Inconsistent(_) => {
            return Err("sol-rpc: getAccountInfo: no consensus".to_string());
        }
    };
    let Some(account) = account else {
        return Err("escrow account does not exist".to_string());
    };
    if account.owner != spec.factory {
        return Err("escrow account is not owned by the factory".to_string());
    }
    let Some(data) = account.data.decode() else {
        return Err("escrow account data does not decode".to_string());
    };
    if data.get(..8) != Some(&ESCROW_ACCOUNT_DISCRIMINATOR[..]) {
        return Err("escrow account is not an Escrow".to_string());
    }
    if data.get(8..40) != Some(donor) {
        return Err("escrow donor does not match the declared birth".to_string());
    }
    if data.get(40..72) != Some(&salt[..]) {
        return Err("escrow salt does not match the declared birth".to_string());
    }
    match data.get(SETTLED_OFFSET) {
        Some(0) => Ok(()),
        Some(_) => Err("escrow already settled".to_string()),
        None => Err("escrow account is too short".to_string()),
    }
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

    // The discriminator every shape carries under the header convention.
    #[test]
    fn escrow_discriminator_is_pinned() {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(b"account:Escrow");
        assert_eq!(digest[..8], ESCROW_ACCOUNT_DISCRIMINATOR);
    }

    #[test]
    fn source_grammar_matches_the_core() {
        assert!(matches!(
            parse_sources("Default:Devnet"),
            Some(RpcSources::Default(SolanaCluster::Devnet))
        ));
        assert!(matches!(
            parse_sources("Default:Mainnet"),
            Some(RpcSources::Default(SolanaCluster::Mainnet))
        ));
        assert!(matches!(
            parse_sources("Custom:http://localhost:8899"),
            Some(RpcSources::Custom(_))
        ));
        assert!(parse_sources("Default:Testnet").is_none());
        assert!(parse_sources("garbage").is_none());
    }

    #[test]
    fn consensus_grammar_matches_the_core() {
        assert!(matches!(
            parse_consensus("equality"),
            Some(ConsensusStrategy::Equality)
        ));
        assert!(matches!(
            parse_consensus("3-of-5"),
            Some(ConsensusStrategy::Threshold {
                min: 3,
                total: Some(5)
            })
        ));
        assert!(parse_consensus("3of5").is_none());
        assert!(parse_consensus("x-of-y").is_none());
    }
}
