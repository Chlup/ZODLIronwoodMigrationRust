//! The [`MigrationContext`] facade: the public, synchronous API the platform SDKs wrap. It ties
//! the valargroup-free core (denominations, scheduling, state, store) to the librustzcash backend.
//! Methods are wired across later tasks.

use crate::error::MigrationError;
use crate::types::Network;

/// Holds wallet context for migration operations (mirrors how `libzcashlc` passes a db path +
/// network + account uuid). Open and operate per call; no shared mutable state.
pub struct MigrationContext {
    #[allow(dead_code)]
    db_path: String,
    #[allow(dead_code)]
    network: Network,
    #[allow(dead_code)]
    account_uuid: [u8; 16],
}

impl MigrationContext {
    /// Create a context bound to a wallet database, network, and account.
    pub fn new(
        db_path: &str,
        network: Network,
        account_uuid: [u8; 16],
    ) -> Result<Self, MigrationError> {
        Ok(Self {
            db_path: db_path.to_string(),
            network,
            account_uuid,
        })
    }
}
