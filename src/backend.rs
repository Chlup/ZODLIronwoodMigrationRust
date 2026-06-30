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

// ======================== Propose + sign (bucketed anchor, V6) ========================
// The §8 mechanism: spend a reserved pre-split note at a bucket-aligned anchor, emitting one
// Ironwood (V6) output. Compile-verified against the real APIs; exercising requires a seeded,
// synced wallet database (a documented integration gap).

use std::collections::BTreeSet;

use zcash_client_backend::data_api::wallet::input_selection::{GreedyInputSelector, InputSelector};
use zcash_client_backend::data_api::wallet::{create_proposed_transactions, SpendingKeys, TargetHeight};
use zcash_client_backend::fees::zip317::MultiOutputChangeStrategy;
use zcash_client_backend::fees::{DustOutputPolicy, SplitPolicy};
use zcash_client_backend::proposal::Proposal;
use zcash_client_backend::wallet::OvkPolicy;
use zcash_client_sqlite::ReceivedNoteId;
use zcash_primitives::transaction::fees::zip317::FeeRule as Zip317FeeRule;
use zcash_primitives::transaction::{TxId, TxVersion};
use zcash_protocol::ShieldedProtocol;
use zip321::TransactionRequest;

use crate::reserved_source::ReservedInputSource;

/// A signed migration transaction ready for the platform to broadcast.
pub(crate) struct SignedTx {
    pub txid: TxId,
    pub raw_tx: Vec<u8>,
}

/// Propose a single migration transfer: spend reserved notes (excluding locked ones) at the
/// bucket-aligned `anchor` and emit one Ironwood (V6) output described by `request`.
pub(crate) fn propose_migration_transfer<'a>(
    db: &'a Db,
    network: &consensus::Network,
    account: AccountUuid,
    request: TransactionRequest,
    reserved: &'a BTreeSet<ReceivedNoteId>,
    migration_locks: &'a BTreeSet<(String, u32)>,
    target_height: TargetHeight,
    anchor: BlockHeight,
) -> Result<Proposal<Zip317FeeRule, ReceivedNoteId>, MigrationError> {
    let reserved_db = ReservedInputSource {
        inner: db,
        reserved,
        migration_locks,
    };
    let change_strategy = MultiOutputChangeStrategy::<Zip317FeeRule, ReservedInputSource<'a, Db>>::new(
        Zip317FeeRule::standard(),
        None,
        ShieldedProtocol::Orchard,
        DustOutputPolicy::default(),
        SplitPolicy::single_output(),
    );
    let input_selector = GreedyInputSelector::<ReservedInputSource<'a, Db>>::new();
    input_selector
        .propose_transaction(
            network,
            &reserved_db,
            target_height,
            anchor,
            ConfirmationsPolicy::default(),
            account,
            request,
            &change_strategy,
            Some(TxVersion::V6),
        )
        .map_err(|e| MigrationError::Backend(format!("propose migration transfer: {e}")))
}

/// Sign a proposed migration transaction with the USK (software signing, no-op Sapling provers)
/// and return the signed transaction id plus its consensus-encoded bytes for broadcast.
pub(crate) fn sign_proposal(
    db: &mut Db,
    network: &consensus::Network,
    usk: &UnifiedSpendingKey,
    proposal: &Proposal<Zip317FeeRule, ReceivedNoteId>,
) -> Result<SignedTx, MigrationError> {
    let spending_keys = SpendingKeys::from_unified_spending_key(usk.clone());
    let txids = create_proposed_transactions::<_, _, std::convert::Infallible, _, std::convert::Infallible, _>(
        db,
        network,
        &NoOpSpendProver,
        &NoOpOutputProver,
        &spending_keys,
        OvkPolicy::Sender,
        proposal,
        Some(TxVersion::V6),
    )
    .map_err(|e| MigrationError::Backend(format!("sign migration tx: {e:?}")))?;
    let txid = *txids.first();
    let raw_tx = raw_transaction(db, txid)?;
    Ok(SignedTx { txid, raw_tx })
}

/// Fetch a transaction from the wallet database and consensus-encode it.
fn raw_transaction(db: &Db, txid: TxId) -> Result<Vec<u8>, MigrationError> {
    let tx = db
        .get_transaction(txid)
        .map_err(|e| MigrationError::Backend(format!("read signed tx: {e:?}")))?
        .ok_or_else(|| MigrationError::Backend("signed transaction not found".to_string()))?;
    let mut bytes = Vec::new();
    tx.write(&mut bytes)
        .map_err(|e| MigrationError::Backend(format!("encode signed tx: {e}")))?;
    Ok(bytes)
}

// ======================== No-op Sapling Provers ========================
// Ported from vizor send.rs. Orchard→Ironwood migration transactions contain no Sapling bundle,
// so `create_proposed_transactions` never invokes these. They return no-op values rather than
// shipping the ~50MB Sapling parameters with the SDK. (Logging dropped — the crate has no logger.)

use sapling_crypto::{
    bundle::GrothProofBytes,
    circuit,
    keys::EphemeralSecretKey,
    prover::{OutputProver, SpendProver},
    value::{NoteValue, ValueCommitTrapdoor},
    Diversifier, MerklePath, PaymentAddress, ProofGenerationKey, Rseed,
};

const GROTH_PROOF_SIZE: usize = 192;

pub(crate) struct NoOpSpendProver;

impl SpendProver for NoOpSpendProver {
    type Proof = GrothProofBytes;

    fn prepare_circuit(
        _proof_generation_key: ProofGenerationKey,
        _diversifier: Diversifier,
        _rseed: Rseed,
        _value: NoteValue,
        _alpha: jubjub::Fr,
        _rcv: ValueCommitTrapdoor,
        _anchor: bls12_381::Scalar,
        _merkle_path: MerklePath,
    ) -> Option<circuit::Spend> {
        None
    }

    fn create_proof<R: rand_core::RngCore>(&self, _circuit: circuit::Spend, _rng: &mut R) -> Self::Proof {
        [0u8; GROTH_PROOF_SIZE]
    }

    fn encode_proof(_proof: Self::Proof) -> GrothProofBytes {
        [0u8; GROTH_PROOF_SIZE]
    }
}

pub(crate) struct NoOpOutputProver;

impl OutputProver for NoOpOutputProver {
    type Proof = GrothProofBytes;

    fn prepare_circuit(
        _esk: &EphemeralSecretKey,
        _payment_address: PaymentAddress,
        _rcm: jubjub::Fr,
        _value: NoteValue,
        _rcv: ValueCommitTrapdoor,
    ) -> circuit::Output {
        circuit::Output {
            value_commitment_opening: None,
            payment_address: None,
            commitment_randomness: None,
            esk: None,
        }
    }

    fn create_proof<R: rand_core::RngCore>(&self, _circuit: circuit::Output, _rng: &mut R) -> Self::Proof {
        [0u8; GROTH_PROOF_SIZE]
    }

    fn encode_proof(_proof: Self::Proof) -> GrothProofBytes {
        [0u8; GROTH_PROOF_SIZE]
    }
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
