//! Backend integration against the valargroup librustzcash fork: balance reads, proposals,
//! and signing. Built up across later tasks. This module also asserts, at compile time, that
//! the Ironwood (`zcash_unstable = "nu6.3"`) wallet APIs are linked and visible.

// If the `nu6.3` cfg or the `orchard`/`unstable` features were wrong, this import would fail to
// resolve — so it doubles as a build-time check of the dependency wiring.
#[allow(unused_imports)]
use zcash_client_backend::data_api::wallet::create_orchard_to_ironwood_transaction;
use zcash_primitives::transaction::TxVersion;

/// Compile-time proof that `TxVersion::V6` (the Ironwood transaction version) exists; it is only
/// compiled in under `zcash_unstable = "nu6.3"` (or `"nu7"`).
#[allow(dead_code)]
fn ironwood_tx_version() -> TxVersion {
    TxVersion::V6
}
