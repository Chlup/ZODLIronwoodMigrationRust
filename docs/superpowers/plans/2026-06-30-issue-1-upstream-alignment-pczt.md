# Issue #1 — PCZT pivot + upstream alignment — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reshape `zodl_ironwood_migration` per [issue #1](https://github.com/Chlup/ZODLIronwoodMigrationRust/issues/1): pivot persistence/signing to PCZTs, adopt canonical `zcash_protocol` types, `ext_`-prefix the SQLite schema, and make denominations self-funding with dust kept in Orchard.

**Architecture:** Pure-core modules (types/error/denominations/scheduling/state/store) stay TDD-first and compile under `--no-default-features` (now also linking the lightweight `zcash_protocol`). The feature-gated backend (backend/context) is reworked to the PCZT pipeline and compile-verified against the valargroup fork.

**Tech Stack:** Rust 2021, `zcash_protocol`/`zcash_client_backend`/`zcash_client_sqlite`/`pczt`/`orchard` (valargroup fork, branch `adam/qleak-pr44-orchard-dummy-ciphertexts`), `rusqlite` 0.37 bundled, `serde`.

## Global Constraints

- cfg: `zcash_unstable="nu6.3"` (set in `.cargo/config.toml`; unchanged).
- `zcash_protocol` is a **required** dependency; all other librustzcash crates stay `optional` behind `librustzcash-backend`.
- Core (`--no-default-features`) must compile + pass tests for: types, error, denominations, scheduling, state, store.
- `TRANSFER_FEE_BUFFER_ZATOSHI = 20_000` (= 4 × ZIP-317 marginal fee 5_000; 2 Orchard + 2 Ironwood actions).
- SQLite schema prefix: `ext_ironwood_migration_*` (reserved external prefix per `zcash_client_sqlite` `wallet/init.rs:488`).
- PreparedTx payload is a serialized PCZT (`raw_pczt`), not a raw tx.
- `MigrationError` is a rich error (no serde); FFI marshals via `Display` + `error_code() -> u32`.
- Frequent commits — one per task. End commit messages with the `Co-Authored-By: Claude Opus 4.8` trailer.
- Backend tier is compile-verified (no seeded wallet fixture).

---

### Task 1: Cargo.toml + lib.rs foundation

**Files:** Modify `Cargo.toml`, `src/lib.rs`

**Interfaces — Produces:** `zcash_protocol` available unconditionally.

- [ ] **Step 1:** In `Cargo.toml`, add `zcash_protocol` to the always-on `[dependencies]` (pinned to the same git branch as the backend crates), and remove it from the `librustzcash-backend` feature's `dep:` list and from the optional block. Keep all other librustzcash crates optional.
- [ ] **Step 2:** In `src/lib.rs`, fix the stale doc comment (remove `network` from the core module list; note `Network` now re-exported from `zcash_protocol`).
- [ ] **Step 3:** Run `cargo build --no-default-features` — Expected: PASS (compiles `zcash_protocol`).
- [ ] **Step 4:** Commit: `chore: make zcash_protocol a required dependency`.

---

### Task 2: types.rs — canonical types + serde adapters + raw_pczt (TDD)

**Files:** Modify `src/types.rs`, `src/lib.rs`

**Interfaces — Produces:** `Network = zcash_protocol::consensus::Network`; `TransferProposal { id: String, amount: Zatoshis, anchor_height: BlockHeight, next_executable_after_height: BlockHeight, expiry_height: BlockHeight }`; `MigrationProgress { completed_transfers: u32, total_transfers: u32, remaining_orchard: Zatoshis, next_transfer_ready_at_height: Option<BlockHeight> }`; `PreparedTx { id: String, txid: String, raw_pczt: Vec<u8> }`. Serde adapter modules `serde_zatoshis`, `serde_block_height`.

- [ ] **Step 1 (failing test):** Add round-trip tests asserting `Zatoshis`/`BlockHeight` fields serialize as plain numbers and round-trip, and `PreparedTx.raw_pczt` round-trips:

```rust
#[test]
fn transfer_proposal_round_trips_canonical_types() {
    let t = TransferProposal {
        id: "abc".to_string(),
        amount: Zatoshis::const_from_u64(1_000_000_000),
        anchor_height: BlockHeight::from_u32(2_880_000),
        next_executable_after_height: BlockHeight::from_u32(2_880_288),
        expiry_height: BlockHeight::from_u32(2_880_576),
    };
    let json = serde_json::to_string(&t).unwrap();
    assert!(json.contains("1000000000")); // serialized as a number
    let back: TransferProposal = serde_json::from_str(&json).unwrap();
    assert_eq!(back.amount, t.amount);
    assert_eq!(back.expiry_height, t.expiry_height);
}

#[test]
fn prepared_tx_carries_raw_pczt() {
    let tx = PreparedTx { id: "t1".into(), txid: "deadbeef".into(), raw_pczt: vec![0x50, 0x00] };
    let back: PreparedTx = serde_json::from_str(&serde_json::to_string(&tx).unwrap()).unwrap();
    assert_eq!(back.raw_pczt, tx.raw_pczt);
}
```

- [ ] **Step 2:** Run `cargo test --no-default-features types::` — Expected: FAIL (compile error: fields/types missing).
- [ ] **Step 3 (implement):** Remove `pub enum Network`; add `pub use zcash_protocol::consensus::Network;`. Add adapter modules:

```rust
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;

pub(crate) mod serde_zatoshis {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Zatoshis, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::from(*v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Zatoshis, D::Error> {
        let n = u64::deserialize(d)?;
        Zatoshis::from_u64(n).map_err(serde::de::Error::custom)
    }
}
pub(crate) mod serde_block_height {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &BlockHeight, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(u32::from(*v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BlockHeight, D::Error> {
        Ok(BlockHeight::from_u32(u32::deserialize(d)?))
    }
}
// optional<BlockHeight> adapter analogously (serde_opt_block_height)
```

Apply `#[serde(with = "serde_zatoshis")]` / `#[serde(with = "serde_block_height")]` to the fields. Rename `MigrationProgress.remaining_orchard_zatoshi → remaining_orchard: Zatoshis` and `next_transfer_ready_at_height: Option<BlockHeight>`. Rename `TransferProposal.amount_zatoshi → amount: Zatoshis`, heights → `BlockHeight`. Rename `PreparedTx.raw_tx → raw_pczt`. Update `lib.rs` re-export list (`Network` now comes from the re-export). Update existing round-trip tests for the new field names/types.

- [ ] **Step 4:** Run `cargo test --no-default-features types::` — Expected: PASS.
- [ ] **Step 5:** Commit: `feat: adopt zcash_protocol types in public API; raw_tx -> raw_pczt`.

---

### Task 3: error.rs — rich error + InvalidStateError + error_code (TDD)

**Files:** Modify `src/error.rs`

**Interfaces — Produces:** `enum InvalidStateError { NoActiveRun, WrongPhase { expected: &'static str, found: String }, AlreadyComplete, NotApplicable(&'static str) }`; `MigrationError { NotSynced, NotInitialized, InvalidState(InvalidStateError), Db(rusqlite::Error), #[cfg(feature="librustzcash-backend")] Backend(SqliteClientError) }`; `MigrationError::error_code(&self) -> u32`.

- [ ] **Step 1 (failing test):**

```rust
#[test]
fn error_codes_are_stable_and_display_readable() {
    assert_eq!(MigrationError::NotSynced.error_code(), 1);
    assert_eq!(MigrationError::NotInitialized.error_code(), 2);
    let e = MigrationError::InvalidState(InvalidStateError::NoActiveRun);
    assert_eq!(e.error_code(), 3);
    assert!(e.to_string().to_lowercase().contains("no active"));
    let db: MigrationError = rusqlite::Error::QueryReturnedNoRows.into();
    assert_eq!(db.error_code(), 4);
}
```

- [ ] **Step 2:** Run `cargo test --no-default-features error::` — Expected: FAIL.
- [ ] **Step 3 (implement):** Drop `derive(Clone, PartialEq, Eq, Serialize, Deserialize)`; keep `derive(Debug)`. Define `InvalidStateError` (with `Display`). Define the new `MigrationError` variants (Backend cfg-gated). Impl `Display`, `std::error::Error` (with `source()` for Db/Backend), `error_code()`, `From<rusqlite::Error>`, and (cfg-gated) `From<SqliteClientError>`. Replace the serde-round-trip test with the code/Display tests.
- [ ] **Step 4:** Run `cargo test --no-default-features error::` — Expected: PASS.
- [ ] **Step 5:** Commit: `feat: rich MigrationError with rusqlite/SqliteClientError + error_code`.

---

### Task 4: denominations.rs — self-funding notes + dust→Orchard (TDD)

**Files:** Modify `src/denominations.rs`

**Interfaces — Produces:** `const TRANSFER_FEE_BUFFER_ZATOSHI: u64 = 20_000`; `DenominationPlan { migration_outputs: Vec<u64> /* note values D_i+BUFFER */, crossing_values: Vec<u64> /* D_i */, orchard_change: Option<u64>, prep_fee_zatoshi: u64, total_input_zatoshi: u64, total_migratable_zatoshi: u64 /* sum of crossing_values */ }`; `fn plan_denominations(total_input_zatoshi: u64, prep_fee_zatoshi: u64) -> Result<DenominationPlan, String>`.

- [ ] **Step 1 (failing tests):**

```rust
#[test]
fn each_output_is_power_of_ten_plus_self_funding_buffer() {
    let plan = plan_denominations(1_234_500_000, 0).unwrap();
    assert_eq!(plan.crossing_values, vec![1_000_000_000, 100_000_000, 100_000_000]);
    assert_eq!(plan.migration_outputs, vec![1_000_020_000, 100_020_000, 100_020_000]);
    assert_eq!(plan.total_migratable_zatoshi, 1_200_000_000);
    // 0.345 ZEC minus three 20_000 buffers stays in Orchard:
    assert_eq!(plan.orchard_change, Some(34_440_000));
}

#[test]
fn dust_is_left_in_orchard_never_folded_into_fee() {
    let plan = plan_denominations(100_030_000, 0).unwrap();
    assert_eq!(plan.migration_outputs, vec![100_020_000]);
    assert_eq!(plan.orchard_change, Some(10_000)); // dust kept, not fee
}

#[test]
fn exact_funding_leaves_no_change() {
    let plan = plan_denominations(100_020_000, 0).unwrap();
    assert_eq!(plan.migration_outputs, vec![100_020_000]);
    assert_eq!(plan.orchard_change, None);
}

#[test]
fn sub_one_zec_input_migrates_nothing_keeps_all_in_orchard() {
    let plan = plan_denominations(50_000_000, 0).unwrap();
    assert!(plan.migration_outputs.is_empty());
    assert_eq!(plan.orchard_change, Some(50_000_000));
    assert_eq!(plan.total_migratable_zatoshi, 0);
}

#[test]
fn noops_when_prep_fee_consumes_balance() {
    let plan = plan_denominations(5_000, 10_000).unwrap();
    assert!(plan.migration_outputs.is_empty());
    assert_eq!(plan.orchard_change, None);
}

#[test]
fn rejects_more_than_max_prepared_outputs() {
    let err = plan_denominations(1_999_999_950_000_000, 0).unwrap_err();
    assert!(err.contains("above the 64 note limit"));
}
```

- [ ] **Step 2:** Run `cargo test --no-default-features denominations::` — Expected: FAIL.
- [ ] **Step 3 (implement):** Replace the body with the greedy "largest power-of-ten ZEC `D` with `D + BUFFER ≤ budget`" loop:

```rust
pub(crate) const TRANSFER_FEE_BUFFER_ZATOSHI: u64 = 20_000;

pub(crate) fn plan_denominations(
    total_input_zatoshi: u64,
    prep_fee_zatoshi: u64,
) -> Result<DenominationPlan, String> {
    let empty = |change: Option<u64>| DenominationPlan {
        migration_outputs: Vec::new(), crossing_values: Vec::new(), orchard_change: change,
        prep_fee_zatoshi, total_input_zatoshi, total_migratable_zatoshi: 0,
    };
    if total_input_zatoshi <= prep_fee_zatoshi {
        return Ok(empty(None));
    }
    let mut budget = total_input_zatoshi - prep_fee_zatoshi;
    let mut outputs = Vec::new();
    let mut crossings = Vec::new();
    loop {
        let one = ZATOSHIS_PER_ZEC; // smallest denomination = 1 ZEC
        if budget < one + TRANSFER_FEE_BUFFER_ZATOSHI { break; }
        let mut d = one;
        while d.checked_mul(10).map_or(false, |d10| {
            d10.checked_add(TRANSFER_FEE_BUFFER_ZATOSHI).map_or(false, |c| c <= budget)
        }) {
            d *= 10;
        }
        let note = d + TRANSFER_FEE_BUFFER_ZATOSHI;
        outputs.push(note);
        crossings.push(d);
        budget -= note;
        if outputs.len() > MIGRATION_MAX_PREPARED_NOTES_PER_RUN {
            return Err(format!(
                "Migration plan would create {} prepared notes, above the {} note limit",
                outputs.len(), MIGRATION_MAX_PREPARED_NOTES_PER_RUN));
        }
    }
    let total_migratable_zatoshi = crossings.iter().sum();
    Ok(DenominationPlan {
        migration_outputs: outputs, crossing_values: crossings,
        orchard_change: if budget > 0 { Some(budget) } else { None },
        prep_fee_zatoshi, total_input_zatoshi, total_migratable_zatoshi,
    })
}
```

Remove `MIN_IRONWOOD_MIGRATION_OUTPUT_ZATOSHI` and the old residual/fold-to-fee logic.

- [ ] **Step 4:** Run `cargo test --no-default-features denominations::` — Expected: PASS.
- [ ] **Step 5:** Commit: `feat: self-funding power-of-ten denominations; dust stays in Orchard`.

---

### Task 5: scheduling.rs — canonical types + crossing values (TDD)

**Files:** Modify `src/scheduling.rs`

**Interfaces — Produces:** `build_schedule(run_id: &str, crossing_amounts: &[u64], target_height: u32, natural_anchor: u32, first_delay_blocks: u32) -> MigrationSchedule` building `TransferProposal` with `amount: Zatoshis`, heights as `BlockHeight`. `bucket_anchor` unchanged.

- [ ] **Step 1 (failing test):** Update existing tests to read `.amount` (`Zatoshis`) and `.expiry_height` (`BlockHeight`) instead of the old `u64`/`u32` fields, e.g.:

```rust
let s = build_schedule("run", &[1_000_000_000, 100_000_000], 100, 2_880_100, 288);
assert_eq!(s.transfers[0].amount, Zatoshis::const_from_u64(1_000_000_000));
assert_eq!(u32::from(s.transfers[0].anchor_height), 2_880_000);
```

- [ ] **Step 2:** Run `cargo test --no-default-features scheduling::` — Expected: FAIL.
- [ ] **Step 3 (implement):** In the map closure, wrap with `Zatoshis::const_from_u64(amount)` (or `from_u64`) and `BlockHeight::from_u32(...)`. Keep `next_executable_after_height` arithmetic on `u32` then wrap. `amount` is the crossing value.
- [ ] **Step 4:** Run `cargo test --no-default-features scheduling::` — Expected: PASS.
- [ ] **Step 5:** Commit: `refactor: scheduling builds TransferProposal with canonical types`.

---

### Task 6: store.rs — ext_ prefix + raw_pczt columns (TDD)

**Files:** Modify `src/store.rs`

**Interfaces — Produces:** tables `ext_ironwood_migration_{runs,prepared_notes,prep_tx,pending_txs}`; `PendingTxRow.raw_pczt`, `PrepTxRow.raw_pczt`; `insert_prep_tx(.., raw_pczt: &[u8], ..)`.

- [ ] **Step 1 (failing test):** Update the existing store tests to use `raw_pczt` field names (e.g. `pending(...)` helper sets `raw_pczt: vec![1,2,3]`; `prep_tx_insert_get_and_status` asserts `p.raw_pczt`). Add one test asserting the table name is prefixed:

```rust
#[test]
fn schema_uses_ext_prefix() {
    let conn = db();
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'ext_ironwood_migration_%'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(n, 4);
}
```

- [ ] **Step 2:** Run `cargo test --no-default-features store::` — Expected: FAIL.
- [ ] **Step 3 (implement):** Rename all four `CREATE TABLE`/`CREATE INDEX` and every SQL string from `ironwood_migration_` → `ext_ironwood_migration_`. Rename the `raw_tx` columns → `raw_pczt`; rename struct fields `PendingTxRow.raw_tx → raw_pczt`, `PrepTxRow.raw_tx → raw_pczt`, the `PENDING_COLUMNS` constant, and `insert_prep_tx`'s `raw_tx` parameter → `raw_pczt`. Update the module doc comment. No logic changes.
- [ ] **Step 4:** Run `cargo test --no-default-features store::` — Expected: PASS.
- [ ] **Step 5:** Commit: `refactor: ext_-prefixed schema; raw_tx -> raw_pczt in store`.

---

### Task 7: state.rs + core full build (compile)

**Files:** Modify `src/state.rs` (only if it constructs `MigrationProgress`).

- [ ] **Step 1:** Run `cargo test --no-default-features` — Expected: it compiles state.rs against the new `MigrationProgress`. If `to_state` builds a `MigrationProgress`, fix the field name (`remaining_orchard`) / wrap `Zatoshis`/`BlockHeight`.
- [ ] **Step 2:** Run full core suite `cargo test --no-default-features` — Expected: PASS (all core modules green).
- [ ] **Step 3:** Run `cargo fmt` then `cargo fmt --check` — Expected: clean.
- [ ] **Step 4:** Commit (if state.rs changed): `fix: align state.rs with canonical MigrationProgress`.

---

### Task 8: backend.rs — PCZT pipeline (compile-verified)

**Files:** Modify `src/backend.rs`

**Interfaces — Produces:** `sign_schedule`/`sign_split` return serialized PCZTs; `update_proof`/`refresh_pczt` re-anchor+prove+sign a stored PCZT; `extract_broadcast_tx(pczt_bytes: &[u8]) -> Result<Vec<u8>, MigrationError>`. `pool_balances`, `target_and_anchor`, `consensus_network`, `parse_usk`, `open_wallet` unchanged.

- [ ] **Step 1 (pipeline):** Replace `sign_proposal` (`create_proposed_transactions` → `raw_transaction`) with:
  1. `propose_migration_transfer(...)` → `Proposal` (unchanged GreedyInputSelector at bucketed anchor).
  2. `let pczt = create_pczt_from_proposal_with_tx_version(db, params, account_id, ovk_policy, &proposal, TxVersion::V6)?;`
  3. Prove: build/cache an Orchard `ProvingKey` (lazy per-context via `orchard::circuit::ProvingKey::build()`); drive `pczt::roles::prover::Prover` for the Orchard bundle and `create_ironwood_proof(&pk)` for the Ironwood bundle.
  4. Sign: `pczt::roles::signer::Signer` — authorize the Orchard spends and `sign_ironwood(...)` with the USK-derived spend authorizing key.
  5. `let bytes = pczt.serialize();` compute txid via one extraction; return bytes + txid.
- [ ] **Step 2:** Add `extract_broadcast_tx` using `pczt::roles::tx_extractor::TransactionExtractor::new(pczt).extract()?` → `Transaction` → consensus-encode to `Vec<u8>`.
- [ ] **Step 3:** Add `refresh_pczt`: `Pczt::parse(bytes)?` → updater (`update_ironwood_with` + Orchard updater) sets a fresh bucketed anchor/witness → re-prove → re-sign → `serialize()`.
- [ ] **Step 4 (build):** Run `cargo build` (backend feature). Resolve exact role constructor/method signatures against the compiler until warning-free. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: PCZT pipeline (create/prove/sign/serialize, extract, refresh)`.

---

### Task 9: context.rs — facade updates (compile-verified)

**Files:** Modify `src/context.rs`

**Interfaces — Produces:** `next_due_transfer`/`sign_note_split` return `PreparedTx { raw_pczt }`; new `refresh_stale_transfers(&self) -> Result<u32, MigrationError>` and `extract_broadcast_tx(&self, pczt: &[u8]) -> Result<Vec<u8>, MigrationError>`; signatures use `Zatoshis`/`BlockHeight`; account id wrapped to `AccountUuid` in `new`.

- [ ] **Step 1:** Update the facade to build `PreparedTx` with `raw_pczt` from the stored PCZT bytes; map `store` rows' `raw_pczt` through. Replace `MigrationError::InvalidState("...".into())` call sites with the `InvalidStateError` variants. Use `Zatoshis`/`BlockHeight` where building `MigrationProgress`/`TransferProposal`.
- [ ] **Step 2:** Add `refresh_stale_transfers` (calls `backend::refresh_pczt` for scheduled transfers whose `anchor_height` bucket is stale relative to the current tip; re-persists) and `extract_broadcast_tx` (delegates to backend).
- [ ] **Step 3 (build):** Run `cargo build` and `cargo test` (backend feature) — Expected: PASS, warning-free; core tests still green.
- [ ] **Step 4:** Commit: `feat: facade returns raw_pczt; add refresh_stale_transfers + extract_broadcast_tx`.

---

### Task 10: Final build, fmt, report

**Files:** Modify `FINAL-REPORT.md` (append), create `docs/UPSTREAM-GAP-ANALYSIS.md`.

- [ ] **Step 1:** `cargo test --no-default-features` (core) — PASS; `cargo test` (backend) — PASS; `cargo build` warning-free; `cargo fmt --check` clean.
- [ ] **Step 2:** Create `docs/UPSTREAM-GAP-ANALYSIS.md` with the valargroup-vs-`feat/ironwood` gap list (§2.1).
- [ ] **Step 3:** Append an issue-#1 section to `FINAL-REPORT.md`: what changed (PCZT pivot, canonical types, ext_ schema, denominations), and the **out-of-scope / non-blocking findings**: §2.3 move-off-valargroup, §3.3 zcash_client_backend/maintainership candidates, §6.1 scanner Reference-retention + witness-retry guidance, plus any new gaps (e.g. PCZT pipeline run-verification needs a seeded wallet).
- [ ] **Step 4:** Commit: `docs: issue #1 final report + upstream gap analysis`.

## Self-Review

- **Spec coverage:** D1 PCZT persist → Tasks 8/9; D2 update-proof → Tasks 8/9; D3 canonical types → Tasks 1/2/5/9; D4 serde adapters → Task 2; D5 rich error → Task 3; D6 ext_ → Task 6; D7 denominations → Task 4; out-of-scope → Task 10. All covered.
- **Placeholders:** none — each core task has concrete test + impl code; backend tasks list exact entry-point APIs (compile-verified tier per the approved spec).
- **Type consistency:** `raw_pczt`, `amount: Zatoshis`, `BlockHeight`, `crossing_values`, `error_code`, `InvalidStateError`, `refresh_stale_transfers`/`refresh_pczt`, `extract_broadcast_tx` used consistently across tasks.
