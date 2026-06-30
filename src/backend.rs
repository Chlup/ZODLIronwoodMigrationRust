// Portions ported from vizor-wallet `rust/src/wallet/sync/send.rs`
// (origin/adam/qleak-pr73-orchard-librustzcash), © Chainapsis, Apache-2.0.

//! Backend integration against the valargroup librustzcash fork: opening the wallet database,
//! resolving accounts and spending keys, reading Orchard/Ironwood balances and chain heights, and
//! building migration transactions as **PCZTs** (issue #1): propose → create-PCZT(V6) → prove →
//! sign → serialize. The crate persists the serialized PCZT; the platform extracts the consensus
//! transaction (one call) and broadcasts it.
//!
//! Compile-verified against the real valargroup APIs. Exercising the pipeline against live data
//! requires a synced wallet database (a documented integration gap); the proving-key circuit-version
//! pairing must likewise be confirmed at runtime.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use rand::rngs::OsRng;
use uuid::Uuid;
use zcash_address::ZcashAddress;
use zcash_client_backend::data_api::wallet::input_selection::{GreedyInputSelector, InputSelector};
use zcash_client_backend::data_api::wallet::{
    create_pczt_from_proposal_with_tx_version, ConfirmationsPolicy, TargetHeight,
};
use zcash_client_backend::data_api::WalletRead;
use zcash_client_backend::fees::zip317::MultiOutputChangeStrategy;
use zcash_client_backend::fees::{DustOutputPolicy, SplitPolicy};
use zcash_client_backend::proposal::Proposal;
use zcash_client_backend::wallet::OvkPolicy;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::{AccountUuid, ReceivedNoteId, WalletDb};
use zcash_keys::keys::{Era, UnifiedAddressRequest, UnifiedSpendingKey};
use zcash_primitives::transaction::fees::zip317::FeeRule as Zip317FeeRule;
use zcash_primitives::transaction::{TxId, TxVersion};
use zcash_protocol::consensus::{self, BlockHeight, Parameters};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::ShieldedProtocol;
use zip321::{Payment, TransactionRequest};

use crate::error::MigrationError;
use crate::reserved_source::ReservedInputSource;
use crate::store;
use crate::types::Network;

/// The concrete wallet database handle the backend operates on.
pub(crate) type Db = WalletDb<rusqlite::Connection, consensus::Network, SystemClock, OsRng>;

/// Spendable Orchard balance and total Ironwood balance (zatoshi) for an account.
pub(crate) struct PoolBalances {
    pub orchard_spendable: u64,
    pub ironwood_total: u64,
}

/// A signed migration transaction, carried as a serialized PCZT ready for the platform to extract
/// and broadcast.
pub(crate) struct SignedPczt {
    pub txid: TxId,
    pub raw_pczt: Vec<u8>,
}

/// Convert a raw 16-byte account id into an [`AccountUuid`].
pub(crate) fn account_uuid(bytes: [u8; 16]) -> AccountUuid {
    AccountUuid::from_uuid(Uuid::from_bytes(bytes))
}

/// Parse a Unified Spending Key from its raw byte encoding (Orchard era).
pub(crate) fn parse_usk(bytes: &[u8]) -> Result<UnifiedSpendingKey, MigrationError> {
    UnifiedSpendingKey::from_bytes(Era::Orchard, bytes)
        .map_err(|e| MigrationError::Pipeline(format!("invalid unified spending key: {e:?}")))
}

/// Open the wallet database at `db_path`. `Network` is `zcash_protocol::consensus::Network`, the
/// same type the [`Db`] is parameterised by, so it is passed straight through.
pub(crate) fn open_wallet(db_path: &str, network: Network) -> Result<Db, MigrationError> {
    WalletDb::for_path(db_path, network, SystemClock, OsRng)
        .map_err(|e| MigrationError::Pipeline(format!("open wallet: {e:?}")))
}

/// Read the spendable Orchard and total Ironwood balances for `account`.
pub(crate) fn pool_balances(db: &Db, account: AccountUuid) -> Result<PoolBalances, MigrationError> {
    let summary = db
        .get_wallet_summary(ConfirmationsPolicy::default())?
        .ok_or(MigrationError::NotSynced)?;
    let balance = summary
        .account_balances()
        .get(&account)
        .ok_or(MigrationError::InvalidState(
            crate::error::InvalidStateError::NotApplicable("unknown account"),
        ))?;
    Ok(PoolBalances {
        orchard_spendable: u64::from(balance.orchard_balance().spendable_value()),
        ironwood_total: u64::from(balance.ironwood_balance().total()),
    })
}

/// Read the current target height and the wallet's natural (spendable) anchor height.
pub(crate) fn target_and_anchor(db: &Db) -> Result<(u32, u32), MigrationError> {
    let (target, anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())?
        .ok_or(MigrationError::NotSynced)?;
    Ok((u32::from(BlockHeight::from(target)), u32::from(anchor)))
}

// ======================== Proving keys ========================
// Built once per process (in-memory; no 50 MB params). The migration transaction carries an Orchard
// (V2 spend) bundle and an Ironwood (V3 output) bundle, each proved with its own circuit. The exact
// circuit-version pairing must be confirmed at runtime against a synced wallet.

fn orchard_proving_key() -> &'static orchard::circuit::ProvingKey {
    static PK: OnceLock<orchard::circuit::ProvingKey> = OnceLock::new();
    PK.get_or_init(|| {
        orchard::circuit::ProvingKey::build(
            orchard::BundleProtocol::OrchardPostNu6_3.circuit_version(),
        )
    })
}

fn ironwood_proving_key() -> &'static orchard::circuit::ProvingKey {
    static PK: OnceLock<orchard::circuit::ProvingKey> = OnceLock::new();
    PK.get_or_init(|| {
        orchard::circuit::ProvingKey::build(
            orchard::BundleProtocol::IronwoodPostNu6_3.circuit_version(),
        )
    })
}

// ======================== Propose + build signed PCZT (bucketed anchor, V6) ========================

/// Propose a single migration transfer: spend reserved notes (excluding locked ones) at the
/// bucket-aligned `anchor` and emit one Ironwood (V6) output described by `request`.
#[allow(clippy::too_many_arguments)]
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
    let change_strategy =
        MultiOutputChangeStrategy::<Zip317FeeRule, ReservedInputSource<'a, Db>>::new(
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
        .map_err(|e| MigrationError::Pipeline(format!("propose migration transfer: {e}")))
}

/// Drive the full PCZT pipeline for a proposal: create the PCZT at V6, prove the Orchard and
/// Ironwood bundles, software-sign every Orchard spend with the USK's spend-authorizing key, and
/// serialize. Returns the serialized signed PCZT plus the extracted transaction id.
pub(crate) fn build_signed_pczt(
    db: &mut Db,
    network: &consensus::Network,
    account: AccountUuid,
    usk: &UnifiedSpendingKey,
    proposal: &Proposal<Zip317FeeRule, ReceivedNoteId>,
) -> Result<SignedPczt, MigrationError> {
    // 1. Create the PCZT (V6 forces the Ironwood output).
    let pczt = create_pczt_from_proposal_with_tx_version::<
        _,
        _,
        std::convert::Infallible,
        _,
        std::convert::Infallible,
        _,
    >(
        db,
        network,
        account,
        OvkPolicy::Sender,
        proposal,
        TxVersion::V6,
    )
    .map_err(|e| MigrationError::Pipeline(format!("create pczt: {e:?}")))?;

    // 2. Prove each bundle that requires it.
    let mut prover = pczt::roles::prover::Prover::new(pczt);
    if prover.requires_orchard_proof() {
        prover = prover
            .create_orchard_proof(orchard_proving_key())
            .map_err(|e| MigrationError::Pipeline(format!("orchard proof: {e:?}")))?;
    }
    if prover.requires_ironwood_proof() {
        prover = prover
            .create_ironwood_proof(ironwood_proving_key())
            .map_err(|e| MigrationError::Pipeline(format!("ironwood proof: {e:?}")))?;
    }
    let pczt = prover.finish();

    // 3. Sign every Orchard spend that is ours. Action positions are randomized (qleak), so we try
    // every index and ignore wrong-key actions, terminating on InvalidIndex (the fork's own pattern).
    let mut signer = pczt::roles::signer::Signer::new(pczt)
        .map_err(|e| MigrationError::Pipeline(format!("pczt signer init: {e:?}")))?;
    let ask = orchard::keys::SpendAuthorizingKey::from(usk.orchard());
    for index in 0.. {
        match signer.sign_orchard(index, &ask) {
            Err(pczt::roles::signer::Error::InvalidIndex) => break,
            Ok(())
            | Err(pczt::roles::signer::Error::OrchardSign(
                orchard::pczt::SignerError::WrongSpendAuthorizingKey,
            )) => {}
            Err(e) => return Err(MigrationError::Pipeline(format!("sign orchard: {e:?}"))),
        }
    }
    let pczt = signer.finish();

    // 4. Finalize spends and serialize; extract once to obtain the txid.
    let pczt = pczt::roles::spend_finalizer::SpendFinalizer::new(pczt)
        .finalize_spends()
        .map_err(|e| MigrationError::Pipeline(format!("finalize spends: {e:?}")))?;
    let raw_pczt = pczt.serialize();
    let tx = pczt::roles::tx_extractor::TransactionExtractor::new(pczt)
        .extract()
        .map_err(|e| MigrationError::Pipeline(format!("extract tx: {e:?}")))?;
    Ok(SignedPczt {
        txid: tx.txid(),
        raw_pczt,
    })
}

/// Extract the broadcast-ready consensus transaction bytes from a serialized signed PCZT. The
/// platform calls this (or its own librustzcash binding) immediately before broadcasting.
pub(crate) fn extract_broadcast_tx(raw_pczt: &[u8]) -> Result<Vec<u8>, MigrationError> {
    let pczt = pczt::Pczt::parse(raw_pczt)
        .map_err(|e| MigrationError::Pipeline(format!("parse pczt: {e:?}")))?;
    let tx = pczt::roles::tx_extractor::TransactionExtractor::new(pczt)
        .extract()
        .map_err(|e| MigrationError::Pipeline(format!("extract tx: {e:?}")))?;
    let mut bytes = Vec::new();
    tx.write(&mut bytes)
        .map_err(|e| MigrationError::Pipeline(format!("encode tx: {e}")))?;
    Ok(bytes)
}

/// Build a zip321 request paying `amount` zatoshi to `address`. Pure (no DB/network access) so it is
/// directly unit-tested; the migration is a self-send, so `address` is the account's own unified
/// address resolved by [`self_payment_request`].
fn build_self_payment(
    address: &ZcashAddress,
    amount: u64,
) -> Result<TransactionRequest, MigrationError> {
    let value = Zatoshis::from_u64(amount)
        .map_err(|e| MigrationError::Pipeline(format!("invalid migration amount: {e:?}")))?;
    let payment = Payment::without_memo(address.clone(), value);
    TransactionRequest::new(vec![payment])
        .map_err(|e| MigrationError::Pipeline(format!("construct self-payment request: {e:?}")))
}

/// Build a zip321 request paying `amount` to the account's own current unified address (Ironwood
/// addresses equal the existing Orchard/unified address, so the migration is a self-send).
fn self_payment_request(
    db: &Db,
    network: &consensus::Network,
    account: AccountUuid,
    amount: u64,
) -> Result<TransactionRequest, MigrationError> {
    let address = db
        .get_last_generated_address_matching(account, UnifiedAddressRequest::AllAvailableKeys)?
        .ok_or(MigrationError::InvalidState(
            crate::error::InvalidStateError::NotApplicable(
                "account has no current unified address",
            ),
        ))?
        .to_zcash_address(network.network_type());
    build_self_payment(&address, amount)
}

/// The note references a proposal selected as inputs, so successive transfers can reserve them.
fn proposal_note_refs(proposal: &Proposal<Zip317FeeRule, ReceivedNoteId>) -> Vec<ReceivedNoteId> {
    proposal
        .steps()
        .iter()
        .flat_map(|step| step.shielded_inputs().into_iter())
        .flat_map(|inputs| inputs.notes().iter())
        .map(|note| *note.internal_note_id())
        .collect()
}

fn pending_row(t: &crate::types::TransferProposal, signed: &SignedPczt) -> store::PendingTxRow {
    store::PendingTxRow {
        txid_hex: signed.txid.to_string(),
        raw_pczt: signed.raw_pczt.clone(),
        anchor_height: u32::from(t.anchor_height),
        target_height: u32::from(t.next_executable_after_height),
        next_executable_after_height: u32::from(t.next_executable_after_height),
        expiry_height: u32::from(t.expiry_height),
        value_zatoshi: u64::from(t.amount),
        fee_zatoshi: 0,
        selected_note_txid: String::new(),
        selected_note_output_index: 0,
        selected_note_value: 0,
        status: "scheduled".to_string(),
        metadata_json: "{}".to_string(),
    }
}

/// Build a signed-PCZT for every scheduled transfer at its bucketed anchor and persist it as a
/// pending tx.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_schedule(
    db: &mut Db,
    conn: &rusqlite::Connection,
    network: &consensus::Network,
    account: AccountUuid,
    usk: &UnifiedSpendingKey,
    run_id: &str,
    account_str: &str,
    transfers: &[crate::types::TransferProposal],
) -> Result<(), MigrationError> {
    let (target, _natural_anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())?
        .ok_or(MigrationError::NotSynced)?;
    let locks = store::locked_note_refs(conn, account_str)?;
    let mut reserved: BTreeSet<ReceivedNoteId> = BTreeSet::new();
    for t in transfers {
        let request = self_payment_request(db, network, account, u64::from(t.amount))?;
        let proposal = propose_migration_transfer(
            db,
            network,
            account,
            request,
            &reserved,
            &locks,
            target,
            BlockHeight::from(t.anchor_height),
        )?;
        reserved.extend(proposal_note_refs(&proposal));
        let signed = build_signed_pczt(db, network, account, usk, &proposal)?;
        store::insert_pending_txs(conn, run_id, &[pending_row(t, &signed)])?;
    }
    Ok(())
}

/// Build, sign (as a PCZT), and persist the note-split (denomination prep) transaction: one
/// self-send creating the planned output denominations.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_split(
    db: &mut Db,
    conn: &rusqlite::Connection,
    network: &consensus::Network,
    account: AccountUuid,
    usk: &UnifiedSpendingKey,
    run_id: &str,
    account_str: &str,
    outputs: &[u64],
) -> Result<SignedPczt, MigrationError> {
    let (target, natural_anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())?
        .ok_or(MigrationError::NotSynced)?;
    let locks = store::locked_note_refs(conn, account_str)?;
    let reserved: BTreeSet<ReceivedNoteId> = BTreeSet::new();
    let total: u64 = outputs.iter().sum();
    let request = self_payment_request(db, network, account, total)?;
    let proposal = propose_migration_transfer(
        db,
        network,
        account,
        request,
        &reserved,
        &locks,
        target,
        natural_anchor,
    )?;
    let signed = build_signed_pczt(db, network, account, usk, &proposal)?;
    store::insert_prep_tx(
        conn,
        run_id,
        &signed.txid.to_string(),
        &signed.raw_pczt,
        "pending",
    )?;
    let prepared: Vec<store::PreparedNote> = outputs
        .iter()
        .enumerate()
        .map(|(i, &value_zatoshi)| store::PreparedNote {
            txid_hex: signed.txid.to_string(),
            output_index: i as u32,
            value_zatoshi,
            note_version: 2,
            nullifier_hex: None,
            lock_state: "locked".to_string(),
        })
        .collect();
    store::insert_prepared_notes(conn, run_id, &prepared)?;
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_self_payment_creates_single_payment_for_amount() {
        let address: ZcashAddress =
            "ztestsapling1ctuamfer5xjnnrdr3xdazenljx0mu0gutcf9u9e74tr2d3jwjnt0qllzxaplu54hgc2tyjdc2p6"
                .parse()
                .expect("address parses");
        let req = build_self_payment(&address, 100_000_000).expect("request builds");
        assert_eq!(req.payments().len(), 1);
        let payment = req.payments().values().next().expect("one payment");
        assert_eq!(payment.amount().map(u64::from), Some(100_000_000));
    }
}
