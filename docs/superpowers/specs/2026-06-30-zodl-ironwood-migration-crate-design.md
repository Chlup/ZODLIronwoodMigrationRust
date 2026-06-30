# Design Spec — `zodl_ironwood_migration` crate

**Date:** 2026-06-30
**Repo:** `ZODLIronwoodMigrationRust` · **Crate:** `zodl_ironwood_migration` (pure-Rust library)
**Source of truth for behaviour:** vizor-wallet `origin/adam/qleak-pr73-orchard-librustzcash` (Apache-2.0)
**Builds against:** valargroup `librustzcash` @ `adam/qleak-pr44-orchard-dummy-ciphertexts` (rev `0c3ad735`)
**Handoff:** `../../../IRONWOOD-MIGRATION-CRATE-HANDOFF.md`

---

## 1. Goal

A shared, pure-Rust library crate implementing the Orchard→Ironwood migration engine, ported from
vizor, exposing a clean **synchronous, network-free, `serde`-derivable** API that both the iOS SDK
(`libzcashlc`) and Android SDK (`backend-lib`) wrap with thin FFI/JNI glue. The crate owns **all
migration decisions** (split sizing, anchor bucketing, scheduling, ordering, state, recovery); the
platform layers own only OS scheduling, UI, and the **network send**.

---

## 2. Key decisions (locked)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | **cfg gate = `zcash_unstable="nu6.3"`** (NOT `nu7`) | The proven `qleak-pr44` branch gates the entire Ironwood wallet layer on `nu6.3` (227 hits in `zcash_client_backend` + 89 in `zcash_client_sqlite` vs 1+6 for `nu7`); `NetworkUpgrade::Nu6_3` is documented as "Ironwood / NU6.3"; vizor's own `Cargo.toml` declares `check-cfg = ['cfg(zcash_unstable, values("nu6.3"))']`. **Corrects handoff §10.** Using `nu7` would compile out every Ironwood API. |
| D2 | **Crate prepares + signs + persists; platform broadcasts** | The crate stays synchronous, runtime-free, and network-free (handoff design rule). Tor + the LWD connection already live in the native SDKs. The crate hands the platform pre-signed tx bytes and records the result — no async runtime, no FFI re-entrant callback. (Refinement of the "no-broadcast-in-crate" option.) |
| D3 | **Note-split = vizor's deterministic power-of-10** | Proven end-to-end, ships with a full unit-test suite, natural-looking denominations. Diverges from the app docs' "randomised ~10 notes / 20k cap" wording — flagged in the final report. |
| D4 | **v1 = compile against real valargroup + fully TDD the pure core + document end-to-end gaps** | Pure-logic core (split, bucketing, scheduling, state-map, reservation filter, persistence) gets full TDD. Integration methods are implemented against real APIs and compile-verified; anything needing a live synced wallet/LWD to exercise is documented in the final report. No mock-only stubs hiding logic. |
| D5 | **Persistence = own SQLite tables in the wallet DB** (lean Rust) | Maximum sharing across SDKs; mirrors vizor. Tables are additive (`ironwood_migration_*`, `CREATE TABLE IF NOT EXISTS`), independent of `zcash_client_sqlite`'s migration system. |
| D6 | **Software-only signing; skip Keystone/PCZT child-signing** | The app contract implies SDK-internal software signing. Sign via `create_proposed_transactions` with the injected USK. Drop vizor's `_signed_child_pczts` table + PCZT roles. |
| D7 | **Pin valargroup by rev `0c3ad735`; orchard by rev** | Reproducibility (handoff §11 Q4). `Cargo.lock` is committed. |

---

## 3. Crate architecture

Two layers separated by a Cargo **feature** so the pure core is fast to compile and test
**without** the heavy valargroup graph, while the default build links the real backend.

```
src/
  lib.rs            # public surface + re-exports; module wiring
  types.rs          # serde data types (proposals, schedule, progress, enums)   [core]
  error.rs          # MigrationError                                            [core]
  denominations.rs  # plan_denominations + DenominationPlan (power-of-10)       [core]  ← TDD
  scheduling.rs     # anchor bucketing + height-based schedule build            [core]  ← TDD
  state.rs          # 13 vizor phases → 6 public MigrationState mapping         [core]  ← TDD
  store.rs          # own SQLite tables; run/pending-tx persistence (rusqlite)  [core]  ← TDD
  network.rs        # Network enum → consensus params                           [core]
  reserved_source.rs# ReservedInputSource: InputSource adapter                  [backend]
  backend.rs        # propose / sign / balance calls against WalletDb           [backend]
  context.rs        # MigrationContext facade (ties core + backend together)    [backend]
tests/
  denominations.rs scheduling.rs state.rs store.rs types.rs   # core unit/integration tests
```

- **`[core]` modules** depend only on lightweight crates (`serde`, `serde_json`, `rusqlite` 0.37,
  `rand`, `hex`). They contain **no valargroup types**. Run with
  `cargo test --no-default-features` — fast red/green/refactor, resilient to the heavy build.
- **`[backend]` modules** are behind the default feature `librustzcash-backend` and compile against
  valargroup. `MigrationContext` (the facade) lives here because it needs `WalletDb`.

This is the seam that lets ~70% of the logic be TDD'd quickly while still compiling the integration
against the real Ironwood APIs (satisfies D4).

---

## 4. Public API

All types derive `serde::{Serialize, Deserialize}`, `Debug`, `Clone`. Amounts are `u64` zatoshi;
heights are `u32`. The FFI/JNI glue marshals these (JSON today; the i64/protobuf conversions are the
glue's job, per handoff §6).

### 4.1 Data types

```rust
pub enum Network { Main, Test }                                  // → consensus params

pub struct NetworkPrivacyOptions { pub use_tor: bool, pub submission_endpoint: Option<String> }

pub struct NoteSplitProposal { pub output_notes: Vec<u64>, pub fee: u64 }

pub struct TransferProposal {
    pub id: String,
    pub amount_zatoshi: u64,
    pub anchor_height: u32,
    pub next_executable_after_height: u32,
    pub expiry_height: u32,
}

pub struct MigrationSchedule { pub transfers: Vec<TransferProposal>, pub estimated_duration_hours: u32 }

pub struct MigrationProgress {
    pub completed_transfers: u32,
    pub total_transfers: u32,
    pub remaining_orchard_zatoshi: u64,
    pub next_transfer_ready_at_height: Option<u32>,
}

/// A pre-signed transaction the PLATFORM broadcasts. (D2)
pub struct PreparedTx { pub id: String, pub txid: String, pub raw_tx: Vec<u8> }

pub enum MigrationState {
    NotStarted,
    SplitPendingConfirmation,
    ReadyToPropose,
    InProgress(MigrationProgress),
    RequiresAttention(AttentionReason),
    Complete,
}
pub enum AttentionReason { InvalidTransfer { transfer_id: String }, TransferExpired, SyncRequiredBeforeNext }
pub enum TransferResult  { Success { txid: String }, NetworkError { retryable: bool }, InvalidNote, Expired }
pub enum MigrationError  { NotSynced, InvalidState(String), NotInitialized, Db(String), Backend(String) }
```

### 4.2 `MigrationContext`

```rust
pub struct MigrationContext { /* db_path, network, account_uuid */ }

impl MigrationContext {
    pub fn new(db_path: &str, network: Network, account_uuid: [u8; 16]) -> Result<Self, MigrationError>;

    // state (sync reads; platform polls for Flow<MigrationState>)
    pub fn migration_state(&self) -> Result<MigrationState, MigrationError>;
    pub fn migration_progress(&self) -> Result<Option<MigrationProgress>, MigrationError>;

    // note splitting
    pub fn is_note_split_needed(&self) -> Result<bool, MigrationError>;
    pub fn prepare_note_split(&self) -> Result<NoteSplitProposal, MigrationError>;
    /// Build + sign + persist the split tx; returns bytes for the platform to broadcast. (D2)
    pub fn sign_note_split(&self, proposal: &NoteSplitProposal, usk: &[u8]) -> Result<PreparedTx, MigrationError>;

    // migration proposal
    pub fn propose_migration_transfers(&self) -> Result<MigrationSchedule, MigrationError>;
    /// Pre-sign + persist every transfer (each with its bucketed anchor + staggered send height + expiry).
    pub fn sign_and_store_migration_schedule(&self, schedule: &MigrationSchedule, usk: &[u8]) -> Result<(), MigrationError>;

    // background execution — prepare/record split (D2)
    pub fn is_sync_required_before_next_transfer(&self) -> Result<bool, MigrationError>;
    /// The next height-due pre-signed tx, or None. Platform broadcasts it.
    pub fn next_due_transfer(&self) -> Result<Option<PreparedTx>, MigrationError>;
    /// Record the platform's broadcast outcome; advances the state machine.
    pub fn record_transfer_result(&self, transfer_id: &str, result: TransferResult) -> Result<(), MigrationError>;

    // on-launch reconciliation
    pub fn has_overdue_transfers(&self) -> Result<bool, MigrationError>;
    pub fn has_invalid_transfers(&self) -> Result<bool, MigrationError>;

    // recovery / lifecycle
    pub fn restart_current_migration_step(&self) -> Result<MigrationSchedule, MigrationError>;
    pub fn initialize_post_upgrade(&self) -> Result<(), MigrationError>;
}
```

### 4.3 Kotlin `OrchardMigrationSdk` → crate mapping

The glue composes the broadcast (D2). USK is held by the platform SDK and passed to signing calls only.

| Kotlin member | Crate call(s) + glue responsibility |
|---|---|
| `getMigrationState` / `getMigrationProgress` | `migration_state()` / `migration_progress()` |
| `isNoteSplitNeeded` | `is_note_split_needed()` |
| `prepareNoteSplit` | `prepare_note_split()` |
| `submitNoteSplit(proposal)` | `p = sign_note_split(proposal, usk)` → **glue broadcasts** `p.raw_tx` → `record_transfer_result(p.id, result)` → return result |
| `proposeMigrationTransfers` | `propose_migration_transfers()` |
| `signAndStoreMigrationSchedule(s)` | `sign_and_store_migration_schedule(s, usk)` |
| `isSyncRequiredBeforeNextTransfer` | `is_sync_required_before_next_transfer()` |
| `executeNextPendingTransfer(opts)` | `due = next_due_transfer()` → if `Some`, **glue broadcasts** `due.raw_tx` with `opts` (Tor/endpoint) → `record_transfer_result(due.id, result)` → return result; else `null` |
| `hasOverdueTransfers` / `hasInvalidTransfers` | `has_overdue_transfers()` / `has_invalid_transfers()` |
| `restartCurrentMigrationStep` | `restart_current_migration_step()` |
| `initializePostUpgrade` | `initialize_post_upgrade()` |

---

## 5. Note-split algorithm (port vizor `plan_denominations`, D3)

Greedy **power-of-10** decomposition of the spendable Orchard balance (after reserving the prep fee):

1. If `total_input ≤ prep_fee` → empty plan (no-op).
2. `available = total_input − prep_fee`; `whole_zec = available / 1e8`; `remainder = available % 1e8`.
3. Start denom = largest power of 10 ≤ `whole_zec / 10`; emit one note per unit at each descending
   denom level (`1000, 100, 10, 1` ZEC …). E.g. `12.345 ZEC → [10, 1, 1]` ZEC whole-part notes.
4. Residual (sub-ZEC): if `> migration_fee + min_output` emit as a note; elif `≥ min_output` keep as
   `orchard_change`; else fold into fee (dust).
5. Cap at **64** notes (`MIGRATION_MAX_PREPARED_NOTES_PER_RUN`) → error if exceeded.

`DenominationPlan { migration_outputs, orchard_change, prep_fee_zatoshi, migration_fee_zatoshi,
total_input_zatoshi, total_migratable_zatoshi }`. `ZATOSHIS_PER_ZEC = 100_000_000`.

**Tests:** port vizor's suite verbatim (no-op-on-fee, decimal denominations, residual-as-change,
fee-reserved-before-decomposition, 64-note-cap). These become `tests/denominations.rs`.

`NoteSplitProposal.output_notes = plan.migration_outputs`, `.fee = plan.prep_fee_zatoshi`.

---

## 6. Anchor bucketing + height-based scheduling (the NEW piece, §8)

Vizor de-correlates by **time** (exponential offsets / 180 s window). The Kotlin API is **height**-based.
We replace the time model entirely with a height model. Implemented in `scheduling.rs` (pure, TDD'd).

### 6.1 Bucketed anchor

```
ANCHOR_BUCKET_SIZE = 288                      // ≈ 6 h
bucket_anchor(natural_anchor) = (natural_anchor / 288) * 288
```

The bucket anchor is **shared network-wide** (every wallet migrating in this 6-hour window uses the
same anchor → k-anonymity for "when did this wallet last sync"). It is always ≤ the natural anchor,
i.e. **in the past**, so the note is witnessable now (witness computed at sign time).

### 6.2 Schedule shape

For a schedule of `N` transfers built at current `target_height` with natural anchor `A`:

- **All transfers share** `anchor_height = bucket_anchor(A)`.
  *Resolves the source-doc contradiction:* Path A §2 says transfers "can share the same anchor and
  are de-correlated via different expiry and send height"; the Kotlin doc says the anchor comes from
  "a shared network-wide bucket." We follow §2 + the Kotlin doc. (§4.5's "distinct anchor heights"
  is noted as superseded: pre-signed txs must anchor at heights ≤ now; temporal de-correlation is via
  send height, and bucket-sharing is the actual anchor-privacy mechanism.)
- **De-correlation by send height:** `transfer[i].next_executable_after_height = target_height + i * TRANSFER_CADENCE_BLOCKS`
  (`TRANSFER_CADENCE_BLOCKS = 288`, ≈ 6 h apart). Optional small per-transfer jitter within the bucket.
- **Distinct expiry:** `transfer[i].expiry_height = next_executable_after_height[i] + TRANSFER_EXPIRY_WINDOW_BLOCKS`
  (`= 288`; each tx stays valid ~6 h past its window; distinct because send heights differ).
- `estimated_duration_hours = ceil(N * TRANSFER_CADENCE_BLOCKS / BLOCKS_PER_HOUR)` (`BLOCKS_PER_HOUR≈48`).

"Migrate immediately" = a 1-transfer schedule at `i=0` (send height = now, bucket anchor).

### 6.3 Witness/checkpoint constraint — **security-sensitive, verify during impl**

Pre-signing a transfer commits its Merkle witness at `anchor_height`. The wallet's shardtree must
retain a **checkpoint at the bucket height** (≤ 287 blocks behind the natural anchor). Plan:

- Compute the witness via the `propose_transaction(target_height, anchor_height, …)` →
  `create_proposed_transactions` path, which uses `WalletCommitmentTrees` at the proposal's anchor.
- **Verify** `zcash_client_sqlite` retains checkpoints ≥ 288 deep. If retention is shallower, fall
  back to the nearest available checkpoint ≥ the bucket height, or widen retention. Record the
  finding. This is the one item flagged "get it reviewed" in §8.

---

## 7. Note reservation (port vizor `ReservedInputSource`)

A wrapper implementing `InputSource` over `zcash_client_sqlite::WalletDb`, excluding notes already
reserved in this batch and migration-locked notes:

```rust
struct ReservedInputSource<'a> {
    inner: &'a WalletDb<..>,
    reserved: &'a BTreeSet<ReceivedNoteId>,
    migration_locks: &'a BTreeSet<(String, u32)>,   // (lowercased txid, output index)
}
```

- `merged_excludes(exclude)` = caller excludes ∪ `reserved`, sorted+deduped → passed to inner.
- `note_is_locked(note)` checks `(txid.lowercase(), output_index)` ∈ `migration_locks`.
- `get_spendable_note` / `select_spendable_notes` / `select_unspent_notes` / `get_account_metadata`
  delegate to `inner` with merged excludes, then post-filter Orchard notes by lock. Transparent
  methods delegate unfiltered.

Each migration transfer's proposal reserves a distinct pre-split note, so all `N` transfers are
pre-signed independently (no inter-transfer change dependency — that's why splitting is mandatory).

Migration path uses the **explicit-anchor** route (`GreedyInputSelector::propose_transaction` with the
bucketed anchor + `proposed_version = Some(TxVersion::V6)`), then `create_proposed_transactions` with
the USK to sign. (The high-level `create_orchard_to_ironwood_transaction` uses the natural anchor and
is unsuitable for bucketing; it may back the immediate-migrate path only.)

---

## 8. State machine (13 vizor phases → 6 public states)

`state.rs` maps vizor's phase string → `MigrationState`:

| vizor phase(s) | `MigrationState` |
|---|---|
| (no run row) | `NotStarted` |
| `no_orchard_funds` with prior completion | `Complete` |
| `ready_to_prepare`, `waiting_for_spendable_orchard` | `ReadyToPropose` (pre-split) / `NotStarted` |
| `preparing_denominations`, `waiting_denom_confirmations` | `SplitPendingConfirmation` |
| `ready_to_migrate` | `ReadyToPropose` |
| `broadcast_scheduled`, `broadcasting`, `waiting_migration_confirmations` | `InProgress(progress)` |
| `failed_recoverable` (expiry) | `RequiresAttention(TransferExpired)` |
| invalid-note detected (`orchard>0 && no valid queued`) | `RequiresAttention(InvalidTransfer{..})` |
| change-back-to-Orchard needs sync | `RequiresAttention(SyncRequiredBeforeNext)` |
| `complete` | `Complete` |
| `paused`, `failed_terminal`, `abandoned` | `RequiresAttention` / terminal (defined in impl) |

`MigrationProgress` is computed from `pending_totals_for_run` (completed = confirmed count;
total = scheduled+broadcasted+confirmed; remaining_orchard from balance; next height from the earliest
unsent `next_executable_after_height`).

---

## 9. Persistence (`store.rs`, rusqlite 0.37, additive tables)

Port vizor's tables, renamed and reshaped for the height model; **plaintext** raw-tx BLOBs (the wallet
DB is the app's secure store — drop vizor's `secret_payload` encryption and the Keystone PCZT table).

- `ironwood_migration_runs(run_id PK, account_uuid, network, db_fingerprint, phase, created_at_ms,
  updated_at_ms, prep_txid, target_values_json, last_error)`
- `ironwood_migration_prepared_notes(run_id, txid_hex, output_index, value_zatoshi, note_version,
  nullifier_hex, lock_state, PK(run_id,txid_hex,output_index))`
- `ironwood_migration_prep_tx(run_id PK, txid_hex, raw_tx BLOB, status)`  — the split tx
- `ironwood_migration_pending_txs(run_id, txid_hex PK, raw_tx BLOB, anchor_height, target_height,
  next_executable_after_height, expiry_height, value_zatoshi, fee_zatoshi, selected_note_txid,
  selected_note_output_index, selected_note_value, status, metadata_json)`  — height-scheduled transfers

Created lazily via `CREATE TABLE IF NOT EXISTS` on first context use. Ported run-management functions:
`create_run`, `mark_run_phase`, `prepared_notes_for_run`, `locked_migration_note_refs`,
`insert_pending_txs`, `next_due_transfer` (status='scheduled' AND `next_executable_after_height ≤ tip`),
`mark_pending_broadcasted`/`_confirmed`, `pending_totals_for_run`, `clear_retriable_pending_txs`,
`reconcile_*_confirmations`. Time-based `random_schedule_offsets`/`scheduled_at_ms` are **dropped**.

**TDD:** `tests/store.rs` exercises every transition against a `tempfile` sqlite DB (no valargroup).

---

## 10. Signing & broadcast flow (D2, D6)

- **Sign (in-crate, CPU, needs USK, no network):** `GreedyInputSelector::propose_transaction` (reserved
  source, bucketed anchor, `Some(TxVersion::V6)`) → `create_proposed_transactions(usk, …, Some(V6))`
  → serialize tx → persist raw bytes + txid. Orchard proving keys build in-process (no Sapling params).
- **Broadcast (platform):** glue takes `PreparedTx.raw_tx`, sends via its native LWD/Tor client honoring
  `NetworkPrivacyOptions`, then calls `record_transfer_result`.
- **Record:** maps `TransferResult` → row status + run phase (Success→confirmed-pending/broadcasted;
  NetworkError{retryable}→stay scheduled/retry; InvalidNote→RequiresAttention(InvalidTransfer);
  Expired→failed_recoverable → eligible for `clear_retriable_pending_txs`).

---

## 11. Cargo wiring

`Cargo.toml` (versions + pins mirror vizor `qleak-pr73`, pinned by **rev** `0c3ad735`; orchard by rev):

```toml
[features]
default = ["librustzcash-backend"]
librustzcash-backend = ["dep:zcash_client_backend", "dep:zcash_client_sqlite",
  "dep:zcash_primitives", "dep:zcash_keys", "dep:zcash_protocol", "dep:zcash_proofs", "dep:orchard"]

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rusqlite = { version = "0.37", features = ["bundled"] }   # MUST match zcash_client_sqlite 0.21
rand = "0.8"
hex = "0.4"
# heavy, optional — git pinned by rev 0c3ad735 (valargroup), orchard by rev:
zcash_client_backend = { version="0.23", git="…/valargroup/librustzcash", rev="0c3ad735",
  default-features=false, features=["orchard","transparent-inputs","unstable"], optional=true }
zcash_client_sqlite  = { version="0.21", …, rev="0c3ad735", features=["orchard","transparent-inputs","unstable","serde"], optional=true }
zcash_primitives = { version="0.28", …, rev="0c3ad735", optional=true }
zcash_keys = { version="0.14", …, rev="0c3ad735", features=["orchard"], optional=true }
zcash_protocol = { version="0.9", …, rev="0c3ad735", optional=true }
zcash_proofs = { version="0.28", …, rev="0c3ad735", optional=true }
orchard = { version="0.14", git="https://github.com/zcash/orchard", rev="204d8ce9", optional=true }

[dev-dependencies]
tempfile = "3"

[lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = ['cfg(zcash_unstable, values("nu6.3"))'] }
```

`.cargo/config.toml`:

```toml
[build]
rustflags = ["--cfg", "zcash_unstable=\"nu6.3\""]
```

Exact feature set (esp. whether `unstable`/`pczt`/`lightwalletd-tonic` are needed for
`proposed_version`) is finalized empirically during impl — start lean, add only what the compiler
demands. SDKs later add this crate as a dep + the same `[patch.crates-io]`/rev pins (handoff §9).

---

## 12. TDD plan & implementation order

Red→green→refactor each; commit to `main`. Core steps run `cargo test --no-default-features` (fast).

1. **Scaffold** — `Cargo.toml`, `.cargo/config.toml`, `lib.rs`, module stubs; `cargo build --no-default-features` green.
2. **`denominations.rs`** — port vizor tests (red) → `plan_denominations` (green).
3. **`scheduling.rs`** — tests for `bucket_anchor`, stagger, expiry, duration, edge cases → impl.
4. **`state.rs`** — tests per phase mapping → impl.
5. **`store.rs`** — tempfile-sqlite tests for runs/prepared-notes/pending-txs/locking/transitions → impl.
6. **`types.rs`/`error.rs`** — serde round-trip tests.
7. **Backend bring-up** — add `librustzcash-backend` feature deps; `cargo build` (first heavy build)
   proves the crate compiles against real valargroup (nu6.3).
8. **`reserved_source.rs`** — port `ReservedInputSource`; unit-test the exclusion-merge logic.
9. **`backend.rs`** — propose/sign/balance against `WalletDb`; compile-verified; integration-test where
   a fixture allows, else document the gap.
10. **`context.rs`** — wire all facade methods to core + backend.
11. **Full `cargo build` + `cargo test`**; commit `Cargo.lock`.

---

## 13. Out of scope (v1) / documented-gap policy

- Keystone/hardware PCZT child-signing (D6).
- In-crate networking / Tor / LWD client (D2) — platform's job.
- Voting (`zcash_voting`) — migration doesn't need it.
- Multi-device / stateless recovery (Path A explicitly drops these).
- Methods that compile against real APIs but need a **live synced wallet DB + LWD** to exercise
  end-to-end are landed with real bodies + unit coverage where possible; remaining end-to-end gaps go
  in the **final report**, not left as silent stubs.

---

## 14. Risks & verification items (→ final report)

1. **Anchor checkpoint retention (§6.3)** — confirm shardtree retains ≥288-deep checkpoints so the
   bucketed anchor is witnessable; design the fallback. Privacy/security-sensitive.
2. **Exact valargroup feature flags** for `proposed_version` / proving — resolved at first heavy build.
3. **`rusqlite` unification** — 0.37 must single-version with `zcash_client_sqlite`'s; verified at build.
4. **Split-algo divergence (D3)** from the app docs' "randomised" wording — intended; flagged.
5. **§4.5 "distinct anchors" superseded** by §2 shared-bucket model (§6.2) — flagged for privacy review.
6. **Heavy first build** (~18-crate valargroup graph + orchard + proofs) — slow but verified buildable.

---

## 15. Licensing

Ported files keep Apache-2.0 attribution headers (vizor is Apache-2.0; crate LICENSE already Apache-2.0).
