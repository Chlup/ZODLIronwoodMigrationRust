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
use zcash_client_backend::data_api::wallet::input_selection::{
    GreedyInputSelector, InputSelector, InputSelectorError,
};
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
use zcash_protocol::consensus::{BlockHeight, Parameters};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::ShieldedProtocol;
use zip321::{Payment, TransactionRequest};

use crate::error::MigrationError;
use crate::reserved_source::ReservedInputSource;
use crate::store;

/// The wallet database handle the backend operates on, generic over the consensus [`Parameters`] `P`
/// (so it works for standard Mainnet/Testnet and for a custom network with runtime activation heights).
pub(crate) type Db<P> = WalletDb<rusqlite::Connection, P, SystemClock, OsRng>;

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

/// Open the wallet database at `db_path` with consensus parameters `network` (any [`Parameters`] impl:
/// standard Mainnet/Testnet or a custom network). Passed straight through to [`WalletDb`].
pub(crate) fn open_wallet<P: Parameters>(db_path: &str, network: P) -> Result<Db<P>, MigrationError> {
    WalletDb::for_path(db_path, network, SystemClock, OsRng)
        .map_err(|e| MigrationError::Pipeline(format!("open wallet: {e:?}")))
}

/// Read the spendable Orchard and total Ironwood balances for `account`.
pub(crate) fn pool_balances<P: Parameters>(
    db: &Db<P>,
    account: AccountUuid,
) -> Result<PoolBalances, MigrationError> {
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

/// Parse a display-order (big-endian) txid hex string back into a `TxId` (internal little-endian).
fn txid_from_display_hex(hex: &str) -> Option<TxId> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    bytes.reverse(); // display order -> internal order
    Some(TxId::from_bytes(bytes))
}

/// Whether the wallet has mined the transaction with the given display-order txid. Used to detect
/// that the denomination-split (prep) transaction has confirmed, so the migration can proceed.
pub(crate) fn is_tx_mined<P: Parameters>(
    db: &Db<P>,
    txid_display_hex: &str,
) -> Result<bool, MigrationError> {
    let Some(txid) = txid_from_display_hex(txid_display_hex) else {
        return Ok(false);
    };
    Ok(db.get_tx_height(txid)?.is_some())
}

/// Read the current target height and the wallet's natural (spendable) anchor height.
pub(crate) fn target_and_anchor<P: Parameters>(db: &Db<P>) -> Result<(u32, u32), MigrationError> {
    let (target, anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())?
        .ok_or(MigrationError::NotSynced)?;
    Ok((u32::from(BlockHeight::from(target)), u32::from(anchor)))
}

// ======================== Proving keys ========================
// Built once per process (in-memory; no 50 MB params). The migration transaction carries an Orchard
// (V2 spend) bundle and an Ironwood (V3 output) bundle, each proved with its own circuit; the note
// split carries only an Orchard bundle and proves it with the same `OrchardPostNu6_3` key. The exact
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
pub(crate) fn propose_migration_transfer<'a, P: Parameters>(
    db: &'a Db<P>,
    network: &P,
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
        MultiOutputChangeStrategy::<Zip317FeeRule, ReservedInputSource<'a, Db<P>>>::new(
            Zip317FeeRule::standard(),
            None,
            ShieldedProtocol::Orchard,
            DustOutputPolicy::default(),
            SplitPolicy::single_output(),
        );
    let input_selector = GreedyInputSelector::<ReservedInputSource<'a, Db<P>>>::new();
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

/// The exact "migrate everything" crossing value for the immediate (single-transaction) path: the
/// whole spendable Orchard balance minus the fee to spend all of it into one Ironwood output.
///
/// Unlike the private path there is no note split to self-fund the fee, so the single sweep transfer
/// must account for the fee up front. We let the input selector compute it rather than estimating:
/// a request for the *entire* balance forces every note to be selected and fails with
/// `InsufficientFunds { required = total + fee }`, so `fee = required - available` and the crossing
/// value is `total - fee`. Returns `None` when nothing is migratable (balance at or below the fee).
pub(crate) fn sweep_crossing_value<P: Parameters>(
    db: &Db<P>,
    network: &P,
    account: AccountUuid,
) -> Result<Option<u64>, MigrationError> {
    let total = pool_balances(db, account)?.orchard_spendable;
    if total == 0 {
        return Ok(None);
    }
    let (target, anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())?
        .ok_or(MigrationError::NotSynced)?;

    let request = self_payment_request(db, network, account, total)?;
    let reserved: BTreeSet<ReceivedNoteId> = BTreeSet::new();
    let locks: BTreeSet<(String, u32)> = BTreeSet::new();
    let reserved_db = ReservedInputSource {
        inner: db,
        reserved: &reserved,
        migration_locks: &locks,
    };
    let change_strategy =
        MultiOutputChangeStrategy::<Zip317FeeRule, ReservedInputSource<'_, Db<P>>>::new(
            Zip317FeeRule::standard(),
            None,
            ShieldedProtocol::Orchard,
            DustOutputPolicy::default(),
            SplitPolicy::single_output(),
        );
    let input_selector = GreedyInputSelector::<ReservedInputSource<'_, Db<P>>>::new();

    let fee = match input_selector.propose_transaction(
        network,
        &reserved_db,
        target,
        anchor,
        ConfirmationsPolicy::default(),
        account,
        request,
        &change_strategy,
        Some(TxVersion::V6),
    ) {
        // The whole balance is already proposable (fee somehow covered) — read the actual fee.
        Ok(proposal) => u64::from(proposal.steps().last().balance().fee_required()),
        // Expected: requesting the whole balance falls exactly `fee` short of covering itself.
        Err(InputSelectorError::InsufficientFunds {
            available,
            required,
        }) => u64::from(required).saturating_sub(u64::from(available)),
        Err(e) => {
            return Err(MigrationError::Pipeline(format!(
                "immediate sweep probe: {e}"
            )))
        }
    };

    Ok(total.checked_sub(fee).filter(|crossing| *crossing > 0))
}

/// Drive the full PCZT pipeline for a proposal: create the PCZT at V6, then prove, sign, and
/// serialize. Migration transfers cross Orchard-destined outputs into Ironwood per the fork's
/// `orchard_outputs_to_ironwood` rule; the note split does NOT go through here (see
/// `split::build_split_pczt`).
pub(crate) fn build_signed_pczt<P: Parameters>(
    db: &mut Db<P>,
    network: &P,
    account: AccountUuid,
    usk: &UnifiedSpendingKey,
    proposal: &Proposal<Zip317FeeRule, ReceivedNoteId>,
) -> Result<SignedPczt, MigrationError> {
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
    prove_sign_finalize(pczt, usk)
}

/// Prove, sign, finalize, and serialize an assembled PCZT. Shared by the migration transfers
/// (whose PCZT comes from the fork's `create_pczt_from_proposal_with_tx_version`) and the note
/// split (whose PCZT comes from `split::build_split_pczt`). The Orchard bundle is
/// `OrchardPostNu6_3` in both cases, so one proving key serves; the Ironwood proof only runs when
/// the PCZT carries an Ironwood bundle (migration transfers).
pub(crate) fn prove_sign_finalize(
    pczt: pczt::Pczt,
    usk: &UnifiedSpendingKey,
) -> Result<SignedPczt, MigrationError> {
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

    // Sign every Orchard spend that is ours — including the note split's fabricated zero-value
    // change-spends, which are wallet-controlled. Action positions are randomized (qleak), so we
    // try every index and ignore wrong-key actions, terminating on InvalidIndex.
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
fn self_payment_request<P: Parameters>(
    db: &Db<P>,
    network: &P,
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
pub(crate) fn sign_schedule<P: Parameters>(
    db: &mut Db<P>,
    conn: &rusqlite::Connection,
    network: &P,
    account: AccountUuid,
    usk: &UnifiedSpendingKey,
    run_id: &str,
    account_str: &str,
    transfers: &[crate::types::TransferProposal],
) -> Result<(), MigrationError> {
    let (target, _natural_anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())?
        .ok_or(MigrationError::NotSynced)?;
    // Exclude the run's OWN prepared notes from the lock set — the transfers exist to spend them.
    let locks = store::locked_note_refs(conn, account_str, Some(run_id))?;
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

/// Build, sign (as a PCZT), and persist the note-split (denomination prep) transaction: spend the
/// wallet's V2 notes and fan the value into one **same-address change output** per planned
/// denomination. Change outputs are the one operation the post-NU6.3 cross-address restriction
/// sanctions for retaining V2 value, so the split stays in the Orchard pool on the current
/// (`OrchardPostNu6_3`) circuit. Stored notes carry the residual-adjusted values at their real
/// (shuffled) action indices, so the `(txid, output_index)` refs match what the scanner stores.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_split<P: Parameters>(
    db: &mut Db<P>,
    conn: &rusqlite::Connection,
    network: &P,
    account: AccountUuid,
    usk: &UnifiedSpendingKey,
    run_id: &str,
    account_str: &str,
    outputs: &[u64],
) -> Result<SignedPczt, MigrationError> {
    // Exclude this run's own (not-yet-existing) notes for symmetry; other live runs' stay locked.
    let locks = store::locked_note_refs(conn, account_str, Some(run_id))?;
    let (pczt, placed_outputs) =
        crate::split::build_split_pczt(db, network, account, usk, &locks, outputs)?;
    let signed = prove_sign_finalize(pczt, usk)?;
    store::insert_prep_tx(
        conn,
        run_id,
        &signed.txid.to_string(),
        &signed.raw_pczt,
        "pending",
    )?;
    let prepared: Vec<store::PreparedNote> = placed_outputs
        .iter()
        .map(|&(action_index, value_zatoshi)| store::PreparedNote {
            txid_hex: signed.txid.to_string(),
            output_index: action_index,
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
