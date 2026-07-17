//! auction canister: auctions, lots, chain-read registration, verdict
//! signatures (docs/game-spec.md).
//!
//! The update surface is frozen by the .did allowlist lint. Authorization is
//! a wallet signature, never a principal. The canister moves no money; its
//! single view of the outside world is one account read per registration,
//! through the SOL RPC canister named in config. Stage G0: skeleton only.
