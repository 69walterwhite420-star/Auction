//! Test double of the SOL RPC canister: serves `getAccountInfo` from a book
//! of accounts seeded by an update — something the real canister never
//! allows, which is exactly why this mock lives outside the trusted
//! repositories. `set_failure` puts the read into one of the two shapes a
//! failing chain read takes — providers that disagree and providers that
//! agree on an error — both of which the game must survive without writing.

use std::cell::RefCell;
use std::collections::BTreeMap;

use candid::CandidType;
use serde::Deserialize;
use sol_rpc_types::{
    AccountInfo, GetAccountInfoParams, MultiRpcResult, RpcConfig, RpcError, RpcSources,
};

/// How the next read fails, if at all.
#[derive(CandidType, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Reads answer honestly from the book.
    None,
    /// The providers disagree: `Inconsistent` with nothing to agree on.
    NoConsensus,
    /// The providers agree the call failed: `Consistent(Err(_))`.
    RpcError,
}

thread_local! {
    static ACCOUNTS: RefCell<BTreeMap<String, AccountInfo>> =
        const { RefCell::new(BTreeMap::new()) };
    static FAILURE: RefCell<Failure> = const { RefCell::new(Failure::None) };
}

#[ic_cdk::update]
fn set_account(pubkey: String, account: Option<AccountInfo>) {
    ACCOUNTS.with_borrow_mut(|accounts| match account {
        Some(account) => {
            accounts.insert(pubkey, account);
        }
        None => {
            accounts.remove(&pubkey);
        }
    });
}

#[ic_cdk::update]
fn set_failure(failure: Failure) {
    FAILURE.with_borrow_mut(|cell| *cell = failure);
}

/// The cycles-cost query sol_rpc_client asks before attaching cycles.
#[ic_cdk::query]
#[allow(non_snake_case)]
fn getAccountInfoCyclesCost(
    _sources: RpcSources,
    _config: Option<RpcConfig>,
    _params: GetAccountInfoParams,
) -> Result<u128, RpcError> {
    Ok(0)
}

#[ic_cdk::update]
#[allow(non_snake_case)]
fn getAccountInfo(
    _sources: RpcSources,
    _config: Option<RpcConfig>,
    params: GetAccountInfoParams,
) -> MultiRpcResult<Option<AccountInfo>> {
    match FAILURE.with_borrow(|cell| *cell) {
        Failure::NoConsensus => return MultiRpcResult::Inconsistent(Vec::new()),
        Failure::RpcError => {
            return MultiRpcResult::Consistent(Err(RpcError::ValidationError(
                "mock: the providers agree this read failed".to_string(),
            )));
        }
        Failure::None => {}
    }
    let account =
        ACCOUNTS.with_borrow(|accounts| accounts.get(&params.pubkey.to_string()).cloned());
    MultiRpcResult::Consistent(Ok(account))
}
