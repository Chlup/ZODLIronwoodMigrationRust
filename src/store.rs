// Persistence reshaped from vizor-wallet `rust/src/wallet/sync/migration.rs`
// (origin/adam/qleak-pr73-orchard-librustzcash), © Chainapsis, Apache-2.0.
// vizor encrypts raw transactions via an internal `secret_payload` module and keys pending
// transactions by wall-clock schedule; this port stores plaintext blobs (the wallet DB is the
// app's secure store) and schedules by block height.

//! Persistence of migration runs and pre-signed PCZTs (serialized `pczt::Pczt`) in additive tables
//! (`ext_ironwood_migration_*`) inside the wallet database. Uses `rusqlite` only.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::state::Phase;

/// Phases for which a run is finished; its notes are no longer considered locked, and it is
/// excluded from `active_run`.
const TERMINAL_PHASES: [&str; 3] = ["complete", "failed_terminal", "abandoned"];

const RUN_COLUMNS: &str =
    "run_id, account_uuid, network, phase, prep_txid, target_values_json, last_error";
const PENDING_COLUMNS: &str = "txid_hex, raw_pczt, anchor_height, target_height, \
    next_executable_after_height, expiry_height, value_zatoshi, fee_zatoshi, selected_note_txid, \
    selected_note_output_index, selected_note_value, status, metadata_json";

/// Fields needed to create a new migration run.
pub(crate) struct NewRun<'a> {
    pub run_id: &'a str,
    pub account_uuid: &'a str,
    pub network: &'a str,
    pub db_fingerprint: &'a str,
    pub phase: Phase,
    pub prep_txid: Option<&'a str>,
    pub target_values: &'a [u64],
}

/// A persisted migration run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunRow {
    pub run_id: String,
    pub account_uuid: String,
    pub network: String,
    pub phase: String,
    pub prep_txid: Option<String>,
    pub target_values: Vec<u64>,
    pub last_error: Option<String>,
}

/// A note produced by the denomination prep transaction, locked for migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedNote {
    pub txid_hex: String,
    pub output_index: u32,
    pub value_zatoshi: u64,
    pub note_version: i64,
    pub nullifier_hex: Option<String>,
    pub lock_state: String,
}

/// A scheduled, pre-signed migration transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingTxRow {
    pub txid_hex: String,
    pub raw_pczt: Vec<u8>,
    pub anchor_height: u32,
    pub target_height: u32,
    pub next_executable_after_height: u32,
    pub expiry_height: u32,
    pub value_zatoshi: u64,
    pub fee_zatoshi: u64,
    pub selected_note_txid: String,
    pub selected_note_output_index: u32,
    pub selected_note_value: u64,
    pub status: String,
    pub metadata_json: String,
}

/// The denomination prep (note-split) transaction for a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrepTxRow {
    pub run_id: String,
    pub txid_hex: String,
    pub raw_pczt: Vec<u8>,
    pub status: String,
}

/// Counts of a run's pending transfers by status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingTotals {
    pub scheduled: u32,
    pub broadcasted: u32,
    pub confirmed: u32,
    pub total: u32,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn terminal_in_clause() -> String {
    TERMINAL_PHASES
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_target_values(json: &str) -> Vec<u64> {
    serde_json::from_str(json).unwrap_or_default()
}

fn map_run_row(row: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    let target_values_json: String = row.get("target_values_json")?;
    Ok(RunRow {
        run_id: row.get("run_id")?,
        account_uuid: row.get("account_uuid")?,
        network: row.get("network")?,
        phase: row.get("phase")?,
        prep_txid: row.get("prep_txid")?,
        target_values: parse_target_values(&target_values_json),
        last_error: row.get("last_error")?,
    })
}

fn map_pending_row(row: &rusqlite::Row) -> rusqlite::Result<PendingTxRow> {
    Ok(PendingTxRow {
        txid_hex: row.get(0)?,
        raw_pczt: row.get(1)?,
        anchor_height: row.get(2)?,
        target_height: row.get(3)?,
        next_executable_after_height: row.get(4)?,
        expiry_height: row.get(5)?,
        value_zatoshi: row.get::<_, i64>(6)? as u64,
        fee_zatoshi: row.get::<_, i64>(7)? as u64,
        selected_note_txid: row.get(8)?,
        selected_note_output_index: row.get(9)?,
        selected_note_value: row.get::<_, i64>(10)? as u64,
        status: row.get(11)?,
        metadata_json: row.get(12)?,
    })
}

/// Create the `ext_ironwood_migration_*` tables if they do not yet exist.
pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ext_ironwood_migration_runs (
            run_id TEXT PRIMARY KEY,
            account_uuid TEXT NOT NULL,
            network TEXT NOT NULL,
            db_fingerprint TEXT NOT NULL,
            phase TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            prep_txid TEXT,
            target_values_json TEXT NOT NULL DEFAULT '[]',
            last_error TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_ext_ironwood_migration_runs_active
            ON ext_ironwood_migration_runs(account_uuid, network, phase, created_at_ms);
        CREATE TABLE IF NOT EXISTS ext_ironwood_migration_prepared_notes (
            run_id TEXT NOT NULL,
            txid_hex TEXT NOT NULL,
            output_index INTEGER NOT NULL,
            value_zatoshi INTEGER NOT NULL,
            note_version INTEGER NOT NULL,
            nullifier_hex TEXT,
            lock_state TEXT NOT NULL DEFAULT 'locked',
            PRIMARY KEY (run_id, txid_hex, output_index)
        );
        CREATE TABLE IF NOT EXISTS ext_ironwood_migration_prep_tx (
            run_id TEXT PRIMARY KEY,
            txid_hex TEXT NOT NULL,
            raw_pczt BLOB NOT NULL,
            status TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS ext_ironwood_migration_pending_txs (
            run_id TEXT NOT NULL,
            txid_hex TEXT PRIMARY KEY,
            raw_pczt BLOB NOT NULL,
            anchor_height INTEGER NOT NULL,
            target_height INTEGER NOT NULL,
            next_executable_after_height INTEGER NOT NULL,
            expiry_height INTEGER NOT NULL,
            value_zatoshi INTEGER NOT NULL,
            fee_zatoshi INTEGER NOT NULL,
            selected_note_txid TEXT NOT NULL,
            selected_note_output_index INTEGER NOT NULL,
            selected_note_value INTEGER NOT NULL,
            status TEXT NOT NULL,
            metadata_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ext_ironwood_migration_pending_due
            ON ext_ironwood_migration_pending_txs(run_id, status, next_executable_after_height);",
    )
}

pub(crate) fn insert_run(conn: &Connection, run: &NewRun) -> rusqlite::Result<()> {
    let now = now_ms();
    let target_values_json = serde_json::to_string(run.target_values)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO ext_ironwood_migration_runs
            (run_id, account_uuid, network, db_fingerprint, phase, created_at_ms, updated_at_ms,
             prep_txid, target_values_json, last_error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, NULL)",
        params![
            run.run_id,
            run.account_uuid,
            run.network,
            run.db_fingerprint,
            run.phase.as_str(),
            now,
            run.prep_txid,
            target_values_json,
        ],
    )?;
    Ok(())
}

// Tested run accessor by primary key; the facade resolves runs via `active_run`, so this is
// retained for direct lookups (e.g. by future reconciliation code) but not yet wired in.
#[allow(dead_code)]
pub(crate) fn run_by_id(conn: &Connection, run_id: &str) -> rusqlite::Result<Option<RunRow>> {
    conn.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM ext_ironwood_migration_runs WHERE run_id = ?1"),
        params![run_id],
        map_run_row,
    )
    .optional()
}

pub(crate) fn active_run(
    conn: &Connection,
    account_uuid: &str,
    network: &str,
) -> rusqlite::Result<Option<RunRow>> {
    conn.query_row(
        &format!(
            "SELECT {RUN_COLUMNS} FROM ext_ironwood_migration_runs
             WHERE account_uuid = ?1 AND network = ?2 AND phase NOT IN ({})
             ORDER BY created_at_ms DESC LIMIT 1",
            terminal_in_clause()
        ),
        params![account_uuid, network],
        map_run_row,
    )
    .optional()
}

pub(crate) fn set_phase(
    conn: &Connection,
    run_id: &str,
    phase: Phase,
    last_error: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE ext_ironwood_migration_runs SET phase = ?2, last_error = ?3, updated_at_ms = ?4
         WHERE run_id = ?1",
        params![run_id, phase.as_str(), last_error, now_ms()],
    )?;
    Ok(())
}

pub(crate) fn insert_prepared_notes(
    conn: &Connection,
    run_id: &str,
    notes: &[PreparedNote],
) -> rusqlite::Result<()> {
    for n in notes {
        conn.execute(
            "INSERT OR REPLACE INTO ext_ironwood_migration_prepared_notes
                (run_id, txid_hex, output_index, value_zatoshi, note_version, nullifier_hex, lock_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run_id,
                n.txid_hex,
                n.output_index,
                n.value_zatoshi as i64,
                n.note_version,
                n.nullifier_hex,
                n.lock_state,
            ],
        )?;
    }
    Ok(())
}

/// Locked prepared-note refs of the account's live (non-terminal) runs, excluding `exclude_run_id`
/// when given. A run's own operations pass their run id: the schedule transfers must SPEND the notes
/// the run's split prepared (locks exist to keep *other* migration operations off them), and the
/// split itself predates its notes. Nothing ever unlocks a note; runs release theirs by reaching a
/// terminal phase.
pub(crate) fn locked_note_refs(
    conn: &Connection,
    account_uuid: &str,
    exclude_run_id: Option<&str>,
) -> rusqlite::Result<BTreeSet<(String, u32)>> {
    let sql = format!(
        "SELECT lower(pn.txid_hex), pn.output_index
         FROM ext_ironwood_migration_prepared_notes pn
         INNER JOIN ext_ironwood_migration_runs r ON r.run_id = pn.run_id
         WHERE r.account_uuid = ?1 AND pn.lock_state = 'locked' AND r.phase NOT IN ({})
           AND (?2 IS NULL OR pn.run_id <> ?2)",
        terminal_in_clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![account_uuid, exclude_run_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
    })?;
    rows.collect()
}

pub(crate) fn insert_pending_txs(
    conn: &Connection,
    run_id: &str,
    txs: &[PendingTxRow],
) -> rusqlite::Result<()> {
    for t in txs {
        conn.execute(
            "INSERT OR REPLACE INTO ext_ironwood_migration_pending_txs
                (run_id, txid_hex, raw_pczt, anchor_height, target_height, next_executable_after_height,
                 expiry_height, value_zatoshi, fee_zatoshi, selected_note_txid,
                 selected_note_output_index, selected_note_value, status, metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                run_id,
                t.txid_hex,
                t.raw_pczt,
                t.anchor_height,
                t.target_height,
                t.next_executable_after_height,
                t.expiry_height,
                t.value_zatoshi as i64,
                t.fee_zatoshi as i64,
                t.selected_note_txid,
                t.selected_note_output_index,
                t.selected_note_value as i64,
                t.status,
                t.metadata_json,
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn next_due_transfer(
    conn: &Connection,
    run_id: &str,
    tip_height: u32,
) -> rusqlite::Result<Option<PendingTxRow>> {
    conn.query_row(
        &format!(
            "SELECT {PENDING_COLUMNS} FROM ext_ironwood_migration_pending_txs
             WHERE run_id = ?1 AND status = 'scheduled' AND next_executable_after_height <= ?2
             ORDER BY next_executable_after_height ASC, txid_hex ASC LIMIT 1"
        ),
        params![run_id, tip_height],
        map_pending_row,
    )
    .optional()
}

/// The earliest send height among a run's still-scheduled transfers, or `None` if none remain.
pub(crate) fn next_scheduled_send_height(
    conn: &Connection,
    run_id: &str,
) -> rusqlite::Result<Option<u32>> {
    conn.query_row(
        "SELECT MIN(next_executable_after_height) FROM ext_ironwood_migration_pending_txs
         WHERE run_id = ?1 AND status = 'scheduled'",
        params![run_id],
        |row| row.get::<_, Option<u32>>(0),
    )
}

/// Txids of a run's broadcasted-but-not-yet-confirmed transfers.
pub(crate) fn broadcasted_txids(
    conn: &Connection,
    run_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT txid_hex FROM ext_ironwood_migration_pending_txs
         WHERE run_id = ?1 AND status = 'broadcasted'",
    )?;
    let rows = stmt.query_map(params![run_id], |row| row.get::<_, String>(0))?;
    rows.collect()
}

pub(crate) fn mark_pending_status(
    conn: &Connection,
    txid_hex: &str,
    status: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE ext_ironwood_migration_pending_txs SET status = ?2 WHERE txid_hex = ?1",
        params![txid_hex, status],
    )?;
    Ok(())
}

pub(crate) fn pending_totals(conn: &Connection, run_id: &str) -> rusqlite::Result<PendingTotals> {
    let mut totals = PendingTotals::default();
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) FROM ext_ironwood_migration_pending_txs WHERE run_id = ?1 GROUP BY status",
    )?;
    let rows = stmt.query_map(params![run_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
    })?;
    for r in rows {
        let (status, count) = r?;
        match status.as_str() {
            "scheduled" => totals.scheduled = count,
            "broadcasted" => totals.broadcasted = count,
            "confirmed" => totals.confirmed = count,
            _ => {}
        }
        totals.total += count;
    }
    Ok(totals)
}

pub(crate) fn clear_scheduled_pending(conn: &Connection, run_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM ext_ironwood_migration_pending_txs WHERE run_id = ?1 AND status = 'scheduled'",
        params![run_id],
    )
}

pub(crate) fn insert_prep_tx(
    conn: &Connection,
    run_id: &str,
    txid_hex: &str,
    raw_pczt: &[u8],
    status: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO ext_ironwood_migration_prep_tx (run_id, txid_hex, raw_pczt, status)
         VALUES (?1, ?2, ?3, ?4)",
        params![run_id, txid_hex, raw_pczt, status],
    )?;
    Ok(())
}

pub(crate) fn prep_tx(conn: &Connection, run_id: &str) -> rusqlite::Result<Option<PrepTxRow>> {
    conn.query_row(
        "SELECT run_id, txid_hex, raw_pczt, status FROM ext_ironwood_migration_prep_tx WHERE run_id = ?1",
        params![run_id],
        |row| {
            Ok(PrepTxRow {
                run_id: row.get(0)?,
                txid_hex: row.get(1)?,
                raw_pczt: row.get(2)?,
                status: row.get(3)?,
            })
        },
    )
    .optional()
}

pub(crate) fn set_prep_tx_status(
    conn: &Connection,
    run_id: &str,
    status: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE ext_ironwood_migration_prep_tx SET status = ?2 WHERE run_id = ?1",
        params![run_id, status],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    #[test]
    fn schema_uses_ext_prefix() {
        let conn = db();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name LIKE 'ext_ironwood_migration_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 4);
    }

    fn sample_run(conn: &Connection, run_id: &str, phase: Phase) {
        insert_run(
            conn,
            &NewRun {
                run_id,
                account_uuid: "acct",
                network: "main",
                db_fingerprint: "fp",
                phase,
                prep_txid: None,
                target_values: &[100_000_000, 200_000_000],
            },
        )
        .unwrap();
    }

    fn note(txid: &str, index: u32, lock_state: &str) -> PreparedNote {
        PreparedNote {
            txid_hex: txid.to_string(),
            output_index: index,
            value_zatoshi: 1,
            note_version: 3,
            nullifier_hex: None,
            lock_state: lock_state.to_string(),
        }
    }

    fn pending(txid: &str, send_height: u32, status: &str) -> PendingTxRow {
        PendingTxRow {
            txid_hex: txid.to_string(),
            raw_pczt: vec![1, 2, 3],
            anchor_height: 2_880_000,
            target_height: 1000,
            next_executable_after_height: send_height,
            expiry_height: send_height + 288,
            value_zatoshi: 100,
            fee_zatoshi: 10,
            selected_note_txid: "note".to_string(),
            selected_note_output_index: 0,
            selected_note_value: 110,
            status: status.to_string(),
            metadata_json: "{}".to_string(),
        }
    }

    #[test]
    fn insert_and_read_run_round_trips() {
        let conn = db();
        sample_run(&conn, "r1", Phase::ReadyToMigrate);
        let row = run_by_id(&conn, "r1").unwrap().unwrap();
        assert_eq!(row.run_id, "r1");
        assert_eq!(row.account_uuid, "acct");
        assert_eq!(row.phase, "ready_to_migrate");
        assert_eq!(row.target_values, vec![100_000_000, 200_000_000]);
        assert_eq!(row.last_error, None);
    }

    #[test]
    fn run_by_id_is_none_when_absent() {
        let conn = db();
        assert!(run_by_id(&conn, "missing").unwrap().is_none());
    }

    #[test]
    fn active_run_excludes_terminal_phases() {
        let conn = db();
        sample_run(&conn, "done", Phase::Complete);
        assert!(active_run(&conn, "acct", "main").unwrap().is_none());
        sample_run(&conn, "live", Phase::BroadcastScheduled);
        assert_eq!(
            active_run(&conn, "acct", "main").unwrap().unwrap().run_id,
            "live"
        );
    }

    #[test]
    fn set_phase_updates_phase_and_error() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        set_phase(&conn, "r1", Phase::FailedRecoverable, Some("tx expired")).unwrap();
        let row = run_by_id(&conn, "r1").unwrap().unwrap();
        assert_eq!(row.phase, "failed_recoverable");
        assert_eq!(row.last_error.as_deref(), Some("tx expired"));
    }

    #[test]
    fn locked_note_refs_returns_lowercased_locked_notes_of_live_runs() {
        let conn = db();
        sample_run(&conn, "live", Phase::BroadcastScheduled);
        insert_prepared_notes(
            &conn,
            "live",
            &[note("AABB", 0, "locked"), note("CCDD", 2, "unlocked")],
        )
        .unwrap();
        let refs = locked_note_refs(&conn, "acct", None).unwrap();
        assert!(refs.contains(&("aabb".to_string(), 0)));
        assert!(!refs.contains(&("ccdd".to_string(), 2)));
    }

    #[test]
    fn locked_note_refs_excludes_terminal_runs() {
        let conn = db();
        sample_run(&conn, "done", Phase::Complete);
        insert_prepared_notes(&conn, "done", &[note("AABB", 0, "locked")]).unwrap();
        assert!(locked_note_refs(&conn, "acct", None).unwrap().is_empty());
    }

    #[test]
    fn broadcasted_txids_returns_only_broadcasted_rows_of_the_run() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        insert_pending_txs(
            &conn,
            "r1",
            &[
                pending("t1", 1000, "broadcasted"),
                pending("t2", 1300, "scheduled"),
                pending("t3", 1600, "confirmed"),
            ],
        )
        .unwrap();
        assert_eq!(broadcasted_txids(&conn, "r1").unwrap(), vec!["t1".to_string()]);
        assert!(broadcasted_txids(&conn, "other").unwrap().is_empty());
    }

    // Regression: a run's own locked notes must not be excluded from its own operations — the
    // schedule transfers spend exactly the notes the split prepared. Other live runs' notes stay
    // locked.
    #[test]
    fn locked_note_refs_excludes_the_callers_own_run() {
        let conn = db();
        sample_run(&conn, "mine", Phase::ReadyToMigrate);
        sample_run(&conn, "other", Phase::BroadcastScheduled);
        insert_prepared_notes(&conn, "mine", &[note("AABB", 0, "locked")]).unwrap();
        insert_prepared_notes(&conn, "other", &[note("CCDD", 1, "locked")]).unwrap();
        let refs = locked_note_refs(&conn, "acct", Some("mine")).unwrap();
        assert!(!refs.contains(&("aabb".to_string(), 0)));
        assert!(refs.contains(&("ccdd".to_string(), 1)));
    }

    #[test]
    fn next_due_returns_earliest_scheduled_at_or_below_tip() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        insert_pending_txs(
            &conn,
            "r1",
            &[
                pending("t2", 2000, "scheduled"),
                pending("t1", 1500, "scheduled"),
                pending("t3", 5000, "scheduled"),
            ],
        )
        .unwrap();
        assert_eq!(
            next_due_transfer(&conn, "r1", 1800)
                .unwrap()
                .unwrap()
                .txid_hex,
            "t1"
        );
        assert_eq!(
            next_due_transfer(&conn, "r1", 4000)
                .unwrap()
                .unwrap()
                .txid_hex,
            "t1"
        );
        assert!(next_due_transfer(&conn, "r1", 100).unwrap().is_none());
    }

    #[test]
    fn next_due_skips_non_scheduled() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        insert_pending_txs(&conn, "r1", &[pending("t1", 1500, "broadcasted")]).unwrap();
        assert!(next_due_transfer(&conn, "r1", 9999).unwrap().is_none());
    }

    #[test]
    fn next_due_round_trips_all_fields() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        let row = pending("t1", 1500, "scheduled");
        insert_pending_txs(&conn, "r1", std::slice::from_ref(&row)).unwrap();
        let got = next_due_transfer(&conn, "r1", 9999).unwrap().unwrap();
        assert_eq!(got, row);
    }

    #[test]
    fn next_scheduled_send_height_returns_min_or_none() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        assert_eq!(next_scheduled_send_height(&conn, "r1").unwrap(), None);
        insert_pending_txs(
            &conn,
            "r1",
            &[
                pending("t1", 2000, "scheduled"),
                pending("t2", 1500, "scheduled"),
                pending("t3", 999, "broadcasted"),
            ],
        )
        .unwrap();
        assert_eq!(next_scheduled_send_height(&conn, "r1").unwrap(), Some(1500));
    }

    #[test]
    fn mark_pending_status_transitions() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        insert_pending_txs(&conn, "r1", &[pending("t1", 1500, "scheduled")]).unwrap();
        mark_pending_status(&conn, "t1", "broadcasted").unwrap();
        assert!(next_due_transfer(&conn, "r1", 9999).unwrap().is_none());
        let totals = pending_totals(&conn, "r1").unwrap();
        assert_eq!(totals.broadcasted, 1);
        assert_eq!(totals.scheduled, 0);
    }

    #[test]
    fn pending_totals_counts_by_status() {
        let conn = db();
        sample_run(&conn, "r1", Phase::BroadcastScheduled);
        insert_pending_txs(
            &conn,
            "r1",
            &[
                pending("t1", 1, "confirmed"),
                pending("t2", 2, "scheduled"),
                pending("t3", 3, "scheduled"),
            ],
        )
        .unwrap();
        let t = pending_totals(&conn, "r1").unwrap();
        assert_eq!(t.confirmed, 1);
        assert_eq!(t.scheduled, 2);
        assert_eq!(t.broadcasted, 0);
        assert_eq!(t.total, 3);
    }

    #[test]
    fn clear_scheduled_pending_removes_only_scheduled() {
        let conn = db();
        sample_run(&conn, "r1", Phase::FailedRecoverable);
        insert_pending_txs(
            &conn,
            "r1",
            &[
                pending("t1", 1, "scheduled"),
                pending("t2", 2, "broadcasted"),
            ],
        )
        .unwrap();
        assert_eq!(clear_scheduled_pending(&conn, "r1").unwrap(), 1);
        let t = pending_totals(&conn, "r1").unwrap();
        assert_eq!(t.scheduled, 0);
        assert_eq!(t.broadcasted, 1);
    }

    #[test]
    fn prep_tx_insert_get_and_status() {
        let conn = db();
        sample_run(&conn, "r1", Phase::PreparingDenominations);
        insert_prep_tx(&conn, "r1", "txid1", &[9, 9, 9], "pending").unwrap();
        let p = prep_tx(&conn, "r1").unwrap().unwrap();
        assert_eq!(p.txid_hex, "txid1");
        assert_eq!(p.raw_pczt, vec![9, 9, 9]);
        assert_eq!(p.status, "pending");
        set_prep_tx_status(&conn, "r1", "broadcasted").unwrap();
        assert_eq!(prep_tx(&conn, "r1").unwrap().unwrap().status, "broadcasted");
    }
}
