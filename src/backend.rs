// Portions ported from vizor-wallet `rust/src/wallet/sync/send.rs`
// (origin/adam/qleak-pr73-orchard-librustzcash), © Chainapsis, Apache-2.0.

//! Backend integration against the valargroup librustzcash fork: opening the wallet database,
//! resolving accounts and spending keys, reading Orchard/Ironwood balances and chain heights,
//! and (further below) proposing + signing migration transactions.
//!
//! The read/convert helpers here are compile-verified against the real valargroup APIs; the
//! `ironwood_balance()` call also confirms the `nu6.3` Ironwood wallet layer is linked. Exercising
//! these against live data requires a synced wallet database (a documented integration gap).

use rand::rngs::OsRng;
use uuid::Uuid;
use zcash_client_backend::data_api::wallet::ConfirmationsPolicy;
use zcash_client_backend::data_api::WalletRead;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::{AccountUuid, WalletDb};
use zcash_keys::keys::{Era, UnifiedSpendingKey};
use zcash_protocol::consensus::{self, BlockHeight};

use crate::error::MigrationError;
use crate::types::Network;

/// The concrete wallet database handle the backend operates on.
pub(crate) type Db = WalletDb<rusqlite::Connection, consensus::Network, SystemClock, OsRng>;

/// Spendable Orchard balance and total Ironwood balance (zatoshi) for an account.
pub(crate) struct PoolBalances {
    pub orchard_spendable: u64,
    pub ironwood_total: u64,
}

/// Map the public [`Network`] to librustzcash consensus parameters.
pub(crate) fn consensus_network(network: Network) -> consensus::Network {
    match network {
        Network::Main => consensus::Network::MainNetwork,
        Network::Test => consensus::Network::TestNetwork,
    }
}

/// Convert a raw 16-byte account id into an [`AccountUuid`].
pub(crate) fn account_uuid(bytes: [u8; 16]) -> AccountUuid {
    AccountUuid::from_uuid(Uuid::from_bytes(bytes))
}

/// Parse a Unified Spending Key from its raw byte encoding (Orchard era).
pub(crate) fn parse_usk(bytes: &[u8]) -> Result<UnifiedSpendingKey, MigrationError> {
    UnifiedSpendingKey::from_bytes(Era::Orchard, bytes)
        .map_err(|e| MigrationError::Backend(format!("invalid unified spending key: {e:?}")))
}

/// Open the wallet database at `db_path`.
pub(crate) fn open_wallet(db_path: &str, network: Network) -> Result<Db, MigrationError> {
    WalletDb::for_path(db_path, consensus_network(network), SystemClock, OsRng)
        .map_err(|e| MigrationError::Db(e.to_string()))
}

/// Read the spendable Orchard and total Ironwood balances for `account`.
pub(crate) fn pool_balances(db: &Db, account: AccountUuid) -> Result<PoolBalances, MigrationError> {
    let summary = db
        .get_wallet_summary(ConfirmationsPolicy::default())
        .map_err(|e| MigrationError::Backend(format!("wallet summary: {e:?}")))?
        .ok_or(MigrationError::NotSynced)?;
    let balance = summary
        .account_balances()
        .get(&account)
        .ok_or_else(|| MigrationError::InvalidState("unknown account".to_string()))?;
    Ok(PoolBalances {
        orchard_spendable: u64::from(balance.orchard_balance().spendable_value()),
        ironwood_total: u64::from(balance.ironwood_balance().total()),
    })
}

/// Read the current target height and the wallet's natural (spendable) anchor height.
pub(crate) fn target_and_anchor(db: &Db) -> Result<(u32, u32), MigrationError> {
    let (target, anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())
        .map_err(|e| MigrationError::Backend(format!("chain state: {e:?}")))?
        .ok_or(MigrationError::NotSynced)?;
    Ok((u32::from(BlockHeight::from(target)), u32::from(anchor)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consensus_network_maps_variants() {
        assert!(matches!(
            consensus_network(Network::Main),
            consensus::Network::MainNetwork
        ));
        assert!(matches!(
            consensus_network(Network::Test),
            consensus::Network::TestNetwork
        ));
    }
}
