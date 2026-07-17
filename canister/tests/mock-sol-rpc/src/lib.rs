//! Test double of the SOL RPC canister: serves `getAccountInfo` from a book
//! of accounts seeded by an update — something the real canister never
//! allows, which is exactly why this mock lives outside the trusted
//! repositories. `set_broken(true)` makes every read answer "no consensus",
//! the transport-failure the game must survive without writing anything.

use std::cell::RefCell;
use std::collections::BTreeMap;

use sol_rpc_types::{AccountInfo, GetAccountInfoParams, MultiRpcResult, RpcConfig, RpcSources};

thread_local! {
    static ACCOUNTS: RefCell<BTreeMap<String, AccountInfo>> =
        const { RefCell::new(BTreeMap::new()) };
    static BROKEN: RefCell<bool> = const { RefCell::new(false) };
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
fn set_broken(broken: bool) {
    BROKEN.with_borrow_mut(|cell| *cell = broken);
}

/// The cycles-cost query sol_rpc_client asks before attaching cycles.
#[ic_cdk::query]
#[allow(non_snake_case)]
fn getAccountInfoCyclesCost(
    _sources: RpcSources,
    _config: Option<RpcConfig>,
    _params: GetAccountInfoParams,
) -> Result<u128, sol_rpc_types::RpcError> {
    Ok(0)
}

#[ic_cdk::update]
#[allow(non_snake_case)]
fn getAccountInfo(
    _sources: RpcSources,
    _config: Option<RpcConfig>,
    params: GetAccountInfoParams,
) -> MultiRpcResult<Option<AccountInfo>> {
    if BROKEN.with_borrow(|cell| *cell) {
        return MultiRpcResult::Inconsistent(Vec::new());
    }
    let account = ACCOUNTS.with_borrow(|accounts| accounts.get(&params.pubkey.to_string()).cloned());
    MultiRpcResult::Consistent(Ok(account))
}
