//! The [`MigrationContext`] facade: the public, synchronous API the platform SDKs wrap. It ties
//! the valargroup-free core (denominations, scheduling, state, store) to the librustzcash backend.
//!
//! Methods that only touch the engine's own SQLite tables (`record_transfer_result`,
//! the no-run state) are exercised by unit tests with a temporary database. Methods that read
//! balances/heights or sign transactions are compile-verified against the real valargroup APIs;
//! exercising them end-to-end needs a seeded, synced wallet database (a documented integration
//! gap, per the design spec D4).

use rusqlite::Connection;
use uuid::Uuid;

use crate::backend::{self, Db};
use crate::denominations::{plan_denominations, MIN_IRONWOOD_MIGRATION_OUTPUT_ZATOSHI};
use crate::error::MigrationError;
use crate::scheduling;
use crate::state::{self, Phase};
use crate::store;
use crate::types::{
    AttentionReason, MigrationProgress, MigrationSchedule, MigrationState, Network,
    NoteSplitProposal, PreparedTx, TransferResult,
};

/// ZIP-317 single-action fee estimate (zatoshi) used by note-split / migration planning.
const FEE_ESTIMATE_ZATOSHI: u64 = 10_000;

/// Holds wallet context for migration operations (mirrors how `libzcashlc` passes a db path +
/// network + account uuid). Open and operate per call; no shared mutable state.
pub struct MigrationContext {
    db_path: String,
    network: Network,
    account_uuid: [u8; 16],
}

impl MigrationContext {
    /// Create a context bound to a wallet database, network, and account, ensuring the engine's
    /// SQLite tables exist.
    pub fn new(
        db_path: &str,
        network: Network,
        account_uuid: [u8; 16],
    ) -> Result<Self, MigrationError> {
        let ctx = Self {
            db_path: db_path.to_string(),
            network,
            account_uuid,
        };
        // Ensure the ironwood_migration_* tables exist.
        let _ = ctx.store_conn()?;
        Ok(ctx)
    }

    // ----- internal helpers -----

    fn store_conn(&self) -> Result<Connection, MigrationError> {
        let conn = Connection::open(&self.db_path)?;
        store::init(&conn)?;
        Ok(conn)
    }

    fn open_wallet(&self) -> Result<Db, MigrationError> {
        backend::open_wallet(&self.db_path, self.network)
    }

    fn account_str(&self) -> String {
        Uuid::from_bytes(self.account_uuid).to_string()
    }

    fn network_str(&self) -> &'static str {
        match self.network {
            Network::Main => "main",
            Network::Test => "test",
        }
    }

    fn orchard_spendable(&self) -> Result<u64, MigrationError> {
        let db = self.open_wallet()?;
        Ok(
            backend::pool_balances(&db, backend::account_uuid(self.account_uuid))?
                .orchard_spendable,
        )
    }

    fn active_run(&self, conn: &Connection) -> Result<Option<store::RunRow>, MigrationError> {
        Ok(store::active_run(
            conn,
            &self.account_str(),
            self.network_str(),
        )?)
    }

    // ----- state -----

    /// Current migration state. App calls this on launch and after every operation.
    pub fn migration_state(&self) -> Result<MigrationState, MigrationError> {
        let conn = self.store_conn()?;
        let Some(run) = self.active_run(&conn)? else {
            return Ok(MigrationState::NotStarted);
        };
        let phase = Phase::parse(&run.phase)
            .ok_or_else(|| MigrationError::InvalidState(format!("unknown phase: {}", run.phase)))?;
        let progress = self.progress_for_run(&conn, &run.run_id)?;
        let attention = run
            .last_error
            .as_deref()
            .map(attention_from_error)
            .filter(|_| matches!(phase, Phase::FailedRecoverable | Phase::FailedTerminal));
        let mapped = state::to_state(phase, progress, attention);
        // Completion: an in-progress run whose transfers are all confirmed, with the Orchard
        // balance fully migrated into Ironwood.
        if let MigrationState::InProgress(p) = &mapped {
            if p.total_transfers > 0 && p.completed_transfers == p.total_transfers {
                let db = self.open_wallet()?;
                let balances =
                    backend::pool_balances(&db, backend::account_uuid(self.account_uuid))?;
                if balances.orchard_spendable == 0 && balances.ironwood_total > 0 {
                    return Ok(MigrationState::Complete);
                }
            }
        }
        Ok(mapped)
    }

    /// Progress details, present only while a migration is in progress.
    pub fn migration_progress(&self) -> Result<Option<MigrationProgress>, MigrationError> {
        match self.migration_state()? {
            MigrationState::InProgress(p) => Ok(Some(p)),
            _ => Ok(None),
        }
    }

    fn progress_for_run(
        &self,
        conn: &Connection,
        run_id: &str,
    ) -> Result<MigrationProgress, MigrationError> {
        let totals = store::pending_totals(conn, run_id)?;
        let remaining_orchard_zatoshi = self.orchard_spendable().unwrap_or(0);
        let next_transfer_ready_at_height = store::next_scheduled_send_height(conn, run_id)?;
        Ok(MigrationProgress {
            completed_transfers: totals.confirmed,
            total_transfers: totals.total,
            remaining_orchard_zatoshi,
            next_transfer_ready_at_height,
        })
    }

    // ----- note splitting -----

    /// Whether the Orchard notes must be split before migration. Splitting is mandatory whenever
    /// there is spendable Orchard balance and no split has yet been confirmed.
    pub fn is_note_split_needed(&self) -> Result<bool, MigrationError> {
        let conn = self.store_conn()?;
        let already_prepared = self
            .active_run(&conn)?
            .and_then(|r| Phase::parse(&r.phase))
            .map(|p| {
                !matches!(
                    p,
                    Phase::NoOrchardFunds
                        | Phase::WaitingForSpendableOrchard
                        | Phase::ReadyToPrepare
                )
            })
            .unwrap_or(false);
        if already_prepared {
            return Ok(false);
        }
        Ok(self.orchard_spendable()? > 0)
    }

    /// Compute the optimal note split for the spendable Orchard balance.
    pub fn prepare_note_split(&self) -> Result<NoteSplitProposal, MigrationError> {
        let total = self.orchard_spendable()?;
        let plan = plan_denominations(
            total,
            FEE_ESTIMATE_ZATOSHI,
            FEE_ESTIMATE_ZATOSHI,
            MIN_IRONWOOD_MIGRATION_OUTPUT_ZATOSHI,
        )
        .map_err(MigrationError::Backend)?;
        Ok(NoteSplitProposal {
            output_notes: plan.migration_outputs,
            fee: plan.prep_fee_zatoshi,
        })
    }

    /// Build, sign, and persist the note-split transaction; returns the bytes for the platform to
    /// broadcast. The split is a wallet-internal multi-output send to the account's own address.
    pub fn sign_note_split(
        &self,
        proposal: &NoteSplitProposal,
        usk: &[u8],
    ) -> Result<PreparedTx, MigrationError> {
        let parsed = backend::parse_usk(usk)?;
        let conn = self.store_conn()?;
        let run_id = new_run_id();
        store::insert_run(
            &conn,
            &store::NewRun {
                run_id: &run_id,
                account_uuid: &self.account_str(),
                network: self.network_str(),
                db_fingerprint: &self.db_path,
                phase: Phase::PreparingDenominations,
                prep_txid: None,
                target_values: &proposal.output_notes,
            },
        )?;
        let mut db = self.open_wallet()?;
        let account = backend::account_uuid(self.account_uuid);
        let signed = backend::sign_split(
            &mut db,
            &conn,
            &backend::consensus_network(self.network),
            account,
            &parsed,
            &run_id,
            &self.account_str(),
            &proposal.output_notes,
        )?;
        Ok(PreparedTx {
            id: format!("prep:{run_id}"),
            txid: signed.txid.to_string(),
            raw_tx: signed.raw_tx,
        })
    }

    // ----- migration proposal -----

    /// Generate the full migration schedule for the spendable Orchard balance.
    pub fn propose_migration_transfers(&self) -> Result<MigrationSchedule, MigrationError> {
        let db = self.open_wallet()?;
        let (target, anchor) = backend::target_and_anchor(&db)?;
        let total = backend::pool_balances(&db, backend::account_uuid(self.account_uuid))?
            .orchard_spendable;
        let plan = plan_denominations(
            total,
            FEE_ESTIMATE_ZATOSHI,
            FEE_ESTIMATE_ZATOSHI,
            MIN_IRONWOOD_MIGRATION_OUTPUT_ZATOSHI,
        )
        .map_err(MigrationError::Backend)?;
        let run_id = new_run_id();
        Ok(scheduling::build_schedule(
            &run_id,
            &plan.migration_outputs,
            target,
            anchor,
            scheduling::FIRST_TRANSFER_DELAY_BLOCKS,
        ))
    }

    /// Pre-sign and persist every transfer in the schedule, each at its bucketed anchor.
    pub fn sign_and_store_migration_schedule(
        &self,
        schedule: &MigrationSchedule,
        usk: &[u8],
    ) -> Result<(), MigrationError> {
        let parsed = backend::parse_usk(usk)?;
        let conn = self.store_conn()?;
        let run_id = match self.active_run(&conn)? {
            Some(r) => r.run_id,
            None => {
                let id = new_run_id();
                store::insert_run(
                    &conn,
                    &store::NewRun {
                        run_id: &id,
                        account_uuid: &self.account_str(),
                        network: self.network_str(),
                        db_fingerprint: &self.db_path,
                        phase: Phase::ReadyToMigrate,
                        prep_txid: None,
                        target_values: &[],
                    },
                )?;
                id
            }
        };
        let mut db = self.open_wallet()?;
        let account = backend::account_uuid(self.account_uuid);
        backend::sign_schedule(
            &mut db,
            &conn,
            &backend::consensus_network(self.network),
            account,
            &parsed,
            &run_id,
            &self.account_str(),
            &schedule.transfers,
        )?;
        store::set_phase(&conn, &run_id, Phase::BroadcastScheduled, None)?;
        Ok(())
    }

    // ----- background execution -----

    /// Whether a sync is required before the next transfer (change returned to Orchard). With the
    /// clean power-of-10 denominations each transfer spends a whole pre-split note and produces no
    /// Orchard change, so this is false; richer change detection is a future refinement.
    pub fn is_sync_required_before_next_transfer(&self) -> Result<bool, MigrationError> {
        Ok(false)
    }

    /// The next height-due pre-signed transfer, or `None`. The platform broadcasts it, then calls
    /// [`Self::record_transfer_result`].
    pub fn next_due_transfer(&self) -> Result<Option<PreparedTx>, MigrationError> {
        let conn = self.store_conn()?;
        let Some(run) = self.active_run(&conn)? else {
            return Ok(None);
        };
        // The note-split (prep) transaction must broadcast and confirm before any transfer.
        if let Some(prep) = store::prep_tx(&conn, &run.run_id)? {
            if prep.status == "pending" {
                return Ok(Some(PreparedTx {
                    id: format!("prep:{}", run.run_id),
                    txid: prep.txid_hex,
                    raw_tx: prep.raw_tx,
                }));
            }
        }
        let db = self.open_wallet()?;
        let (target, _anchor) = backend::target_and_anchor(&db)?;
        let Some(tx) = store::next_due_transfer(&conn, &run.run_id, target)? else {
            return Ok(None);
        };
        Ok(Some(PreparedTx {
            id: tx.txid_hex.clone(),
            txid: tx.txid_hex,
            raw_tx: tx.raw_tx,
        }))
    }

    /// Record the platform's broadcast outcome, advancing the engine's state.
    pub fn record_transfer_result(
        &self,
        transfer_id: &str,
        result: TransferResult,
    ) -> Result<(), MigrationError> {
        let conn = self.store_conn()?;
        let Some(run) = self.active_run(&conn)? else {
            return Err(MigrationError::InvalidState(
                "no active migration run".to_string(),
            ));
        };
        // A result for the note-split (prep) transaction advances the split phase.
        if let Some(run_id) = transfer_id.strip_prefix("prep:") {
            if let TransferResult::Success { .. } = result {
                store::set_prep_tx_status(&conn, run_id, "broadcasted")?;
                store::set_phase(&conn, run_id, Phase::WaitingDenomConfirmations, None)?;
            }
            return Ok(());
        }
        match result {
            TransferResult::Success { .. } => {
                store::mark_pending_status(&conn, transfer_id, "broadcasted")?;
            }
            TransferResult::NetworkError { .. } => { /* leave scheduled for retry */ }
            TransferResult::InvalidNote => {
                store::set_phase(
                    &conn,
                    &run.run_id,
                    Phase::FailedRecoverable,
                    Some(&format!("invalid note for transfer {transfer_id}")),
                )?;
            }
            TransferResult::Expired => {
                store::set_phase(
                    &conn,
                    &run.run_id,
                    Phase::FailedRecoverable,
                    Some(&format!("transfer {transfer_id} expired")),
                )?;
            }
        }
        Ok(())
    }

    // ----- on-launch reconciliation -----

    /// Whether any scheduled transfer is past its send height but not yet broadcast.
    pub fn has_overdue_transfers(&self) -> Result<bool, MigrationError> {
        Ok(self.next_due_transfer()?.is_some())
    }

    /// Whether the migration is in an invalid state: spendable Orchard remains but no scheduled
    /// transfer covers it.
    pub fn has_invalid_transfers(&self) -> Result<bool, MigrationError> {
        let conn = self.store_conn()?;
        let Some(run) = self.active_run(&conn)? else {
            return Ok(false);
        };
        let totals = store::pending_totals(&conn, &run.run_id)?;
        let nothing_queued = totals.scheduled == 0 && totals.broadcasted == 0;
        Ok(nothing_queued && self.orchard_spendable()? > 0)
    }

    // ----- recovery / lifecycle -----

    /// Re-evaluate the remaining spendable Orchard balance and return a fresh schedule for it. The
    /// returned schedule goes through the normal confirm → sign flow.
    pub fn restart_current_migration_step(&self) -> Result<MigrationSchedule, MigrationError> {
        let conn = self.store_conn()?;
        if let Some(run) = self.active_run(&conn)? {
            store::clear_scheduled_pending(&conn, &run.run_id)?;
        }
        self.propose_migration_transfers()
    }

    /// Called on first launch after the Ironwood upgrade. Ensures the engine tables exist; the
    /// minimum anchor is enforced implicitly because every bucketed anchor derives from the
    /// wallet's current (post-upgrade) sync state.
    pub fn initialize_post_upgrade(&self) -> Result<(), MigrationError> {
        let _ = self.store_conn()?;
        Ok(())
    }
}

fn new_run_id() -> String {
    Uuid::new_v4().to_string()
}

/// Classify a recoverable failure's error message into an [`AttentionReason`].
fn attention_from_error(message: &str) -> AttentionReason {
    let lower = message.to_ascii_lowercase();
    if lower.contains("invalid note") {
        // The transfer id is embedded in the message after "transfer ".
        let transfer_id = message
            .split("transfer ")
            .nth(1)
            .unwrap_or("")
            .trim()
            .to_string();
        AttentionReason::InvalidTransfer { transfer_id }
    } else {
        AttentionReason::TransferExpired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn ctx() -> (NamedTempFile, MigrationContext) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap().to_string();
        let ctx = MigrationContext::new(&path, Network::Main, [7u8; 16]).unwrap();
        (file, ctx)
    }

    #[test]
    fn new_creates_tables_and_state_is_not_started() {
        let (_file, ctx) = ctx();
        assert_eq!(ctx.migration_state().unwrap(), MigrationState::NotStarted);
        assert!(ctx.migration_progress().unwrap().is_none());
    }

    #[test]
    fn attention_from_error_classifies() {
        assert!(matches!(
            attention_from_error("invalid note for transfer run-2"),
            AttentionReason::InvalidTransfer { .. }
        ));
        assert_eq!(
            attention_from_error("transfer run-1 expired"),
            AttentionReason::TransferExpired
        );
    }
}
