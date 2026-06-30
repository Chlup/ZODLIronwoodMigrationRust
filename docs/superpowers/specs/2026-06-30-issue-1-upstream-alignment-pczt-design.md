# Design — Issue #1 changes: PCZT pivot + upstream-alignment

**Branch:** `feat/issue-1-upstream-alignment`
**Date:** 2026-06-30
**Source:** [Chlup/ZODLIronwoodMigrationRust#1](https://github.com/Chlup/ZODLIronwoodMigrationRust/issues/1) — review notes on
`docs/superpowers/specs/2026-06-30-zodl-ironwood-migration-crate-design.md`.

This builds on the existing crate (the as-built engine described in `FINAL-REPORT.md`). It reshapes the
crate per the colleague's review, with the central change being a pivot from "propose → sign a raw
transaction → persist raw bytes" to "propose → build a **PCZT** → prove → sign → persist the **PCZT**".

## Approved scope (from brainstorming)

| Decision | Choice |
|---|---|
| Change boundary | **This crate only.** Items that "should live in `zcash_client_backend`" and the release requirement to move off the valargroup forks are recorded in the final report, not implemented here. |
| PCZT pivot | **Do it now**, in this spec, alongside the lighter alignment items. |
| §6.1 scanning (Reference retention, witness retry) | **Out of scope** — documented as platform/librustzcash scanner work in the final report. |
| Verification depth | **Compile-verified backend + TDD on the pure core** (same bar as the prior spec; no seeded wallet-DB fixture). |

### Feasibility (confirmed against the valargroup fork)

The full Ironwood PCZT pipeline exists in the pinned `librustzcash` fork:
`zcash_client_backend::data_api::wallet::create_pczt_from_proposal_with_tx_version(…, proposal, TxVersion::V6) -> pczt::Pczt`;
the `pczt` crate carries an `ironwood: orchard::Bundle` threaded through all roles
(`update_ironwood_with`, `create_ironwood_proof`, `sign_ironwood`, tx-extractor `extract() -> Transaction`);
serialization via `Pczt::serialize()` / `Pczt::parse(&[u8])`. So the pivot is implementable today.

---

## Decisions and alternatives

- **D1 — Persist a fully proven+signed PCZT; extract the tx for broadcast.** At prepare/sign time we run
  the proposal → create-PCZT(V6) → prove → sign pipeline and persist `pczt.serialize()`. `PreparedTx`
  carries the serialized PCZT (`raw_pczt`). The platform broadcasts by extracting the transaction from the
  PCZT (one librustzcash call; the iOS/Android SDKs already link librustzcash); the crate also exposes a
  convenience extractor so a non-librustzcash caller can obtain broadcast-ready bytes.
  - *Alternative considered:* persist an **unsigned/un-proven** PCZT and defer prove+sign to broadcast time
    (always-fresh anchor). Rejected as the default because the issue explicitly says "persist the *signed*
    PCZTs," and deferring proving to the broadcast moment puts an expensive, failure-prone step on the
    critical send path. We instead get anchor-freshness from **D2**.
- **D2 — Add an explicit re-anchor/re-prove/re-sign ("update proof") operation** (answers §4.2). A persisted
  PCZT whose bucketed anchor has gone stale (or whose witness was invalidated by a rescan) is refreshed by
  re-running updater → prover → signer with a fresh bucketed anchor, replacing the stored PCZT and txid.
  This is the capability a raw signed tx cannot offer and is the concrete reason to persist PCZTs.
- **D3 — Adopt canonical `zcash_protocol` types in the public API.** `Network`, `Zatoshis`, `BlockHeight`
  replace the hand-rolled `Network` enum and the bare `u64`/`u32` fields. Consequence: **`zcash_protocol`
  becomes a required (non-optional) dependency**; the heavy crates (`zcash_client_backend`,
  `zcash_client_sqlite`, `orchard`, `sapling-crypto`, `zcash_proofs`, …) stay behind the
  `librustzcash-backend` feature. `cargo test --no-default-features` still runs the pure core; it now also
  compiles the lightweight `zcash_protocol`.
- **D4 — FFI marshals the canonical types as plain numbers.** `Zatoshis(u64)` and `BlockHeight(u32)` are
  newtypes without a serde derive, so the public types use `#[serde(with = …)]` adapter modules that
  serialize them as `u64`/`u32`. (If `zcash_protocol` exposes a `serde` feature, enable it instead — the
  adapter is the fallback.) Round-trip tests guard this.
- **D5 — `MigrationError` becomes a rich error; the FFI edge stringifies.** Variants become
  `Db(rusqlite::Error)`, `Backend(SqliteClientError)` (the latter `#[cfg(feature = "librustzcash-backend")]`),
  and `InvalidState(InvalidStateError)` where `InvalidStateError` is a proper enum. Because `rusqlite::Error`
  / `SqliteClientError` are neither `Serialize` nor `Clone`/`PartialEq`, `MigrationError` **drops** the
  `Serialize`/`Deserialize`/`Clone`/`PartialEq`/`Eq` derives; it keeps `Display` + `std::error::Error`, and
  the FFI/JNI glue marshals via `Display` plus a stable `error_code(&self) -> u32`.
- **D6 — `ext_`-prefixed SQLite schema.** Per `zcash_client_sqlite` `wallet/init.rs:488-492` ("schema
  created by external migrations **MUST** use name prefixing … `ext_` is reserved for external names"),
  rename `ironwood_migration_*` → `ext_ironwood_migration_*` (tables + indexes). Pre-release with
  `CREATE TABLE IF NOT EXISTS`, so no data migration — fresh names. This guarantees no collision with
  librustzcash's own current/future `ironwood_*` tables in the shared wallet DB.
- **D7 — Self-funding denominations; dust always stays in Orchard** (§5). Each prepared note holds
  `power_of_ten + TRANSFER_FEE_BUFFER_ZATOSHI` (= `20_000` = 4 × ZIP-317 marginal fee, for 2 Orchard +
  2 Ironwood actions) so it pays its own transfer fee and crosses an exact power-of-ten value. Any residual
  (including sub-threshold dust) is **always** returned as Orchard change — never folded into the fee —
  removing the dust-attack deanonymization vector.

---

## Component design

### `Cargo.toml` / features
- Move `zcash_protocol` out of the optional/`librustzcash-backend` set into the always-on `[dependencies]`
  (still pinned to the valargroup fork by branch). Add its `serde` feature if it exists.
- All other librustzcash crates remain `optional` and gated by `librustzcash-backend` (unchanged).

### `types.rs` (public FFI surface)
- Remove the custom `pub enum Network`; re-export `pub use zcash_protocol::consensus::Network`.
- `TransferProposal`: `amount_zatoshi: u64 → amount: Zatoshis`;
  `anchor_height/next_executable_after_height/expiry_height: u32 → BlockHeight`.
- `MigrationProgress`: `remaining_orchard_zatoshi → Zatoshis`; `next_transfer_ready_at_height → Option<BlockHeight>`.
- `PreparedTx`: `raw_tx: Vec<u8> → raw_pczt: Vec<u8>` (serialized PCZT); keep `id`, `txid`.
- Add `serde_zatoshis` / `serde_block_height` adapter modules (serialize as `u64`/`u32`). Keep all existing
  derives + round-trip tests; extend tests to cover the new types.

### `error.rs`
- New shape per D5: `NotSynced`, `NotInitialized`, `InvalidState(InvalidStateError)`, `Db(rusqlite::Error)`,
  `Backend(SqliteClientError)` (cfg-gated). Add `InvalidStateError` enum (e.g. `NoActiveRun`,
  `WrongPhase { expected, found }`, `AlreadyComplete`, …). Drop serde/Clone/Eq derives; add
  `error_code(&self) -> u32` for the FFI. Keep `From<rusqlite::Error>`; add
  `From<SqliteClientError>` (cfg-gated). Update tests (remove the serde-round-trip test, add code/Display tests).

### `denominations.rs` (D7) — TDD
- Add `pub(crate) const TRANSFER_FEE_BUFFER_ZATOSHI: u64 = 20_000;` (document = 4 × `MARGINAL_FEE` 5_000).
- New algorithm: greedily pick the largest power-of-ten ZEC denomination `D` with `D + BUFFER ≤ remaining
  budget`; record a prepared note of value `D + BUFFER` (crossing value `D`); subtract `D + BUFFER`. When no
  whole-ZEC denomination fits, **all** remaining budget becomes `orchard_change` (never fee). Cap at
  `MIGRATION_MAX_PREPARED_NOTES_PER_RUN`.
- `DenominationPlan`: `migration_outputs` now holds note values (`D_i + BUFFER`); add `crossing_values`
  (`D_i`) so scheduling/transfers know the exact turnstile amount; `orchard_change: Option<u64>` is `Some`
  for any positive residual. Drop **both** the `migration_fee_zatoshi` and `minimum_output_zatoshi` parameters
  (the per-note fee is now the `TRANSFER_FEE_BUFFER_ZATOSHI` constant) and the fold-to-fee branch; simplify
  the signature to `plan_denominations(total_input_zatoshi, prep_fee_zatoshi)`.
- Rewrite the 5 ported tests around the new semantics (self-funding values, dust→Orchard).

### `store.rs` (D6, D1)
- Rename all four tables + two indexes `ironwood_migration_* → ext_ironwood_migration_*` (SQL + doc comments).
- Rename `raw_tx` columns → `raw_pczt` in `ext_ironwood_migration_prep_tx` and
  `ext_ironwood_migration_pending_txs`; rename the corresponding struct fields (`PendingTxRow.raw_tx →
  raw_pczt`, `PrepTxRow.raw_tx → raw_pczt`) and the `insert_prep_tx` / `PENDING_COLUMNS` plumbing.
- No behavioral change to scheduling/locking/queries. Update the 14 store tests for the new names.

### `backend.rs` (D1, D2) — the PCZT pivot, compile-verified
- Replace the `propose_migration_transfer` + `sign_proposal` (`create_proposed_transactions` → `raw_transaction`)
  path with a PCZT pipeline:
  1. **Propose** — unchanged `GreedyInputSelector::propose_transaction` at the bucketed anchor, producing a
     `Proposal`.
  2. **Create PCZT** — `create_pczt_from_proposal_with_tx_version(db, params, account_id, ovk_policy,
     &proposal, TxVersion::V6) -> Pczt`.
  3. **Prove** — drive the prover roles for the Orchard (V2 spend) bundle and the Ironwood (V3 output)
     bundle (`create_ironwood_proof(&pk)` + the Orchard proof). Build/cache the Orchard `ProvingKey` once
     per context (in-memory `ProvingKey::build()`; no 50 MB params — Sapling is never involved).
  4. **Sign** — software-sign with the USK-derived authorizing key via the signer roles (`sign_ironwood` +
     Orchard spend auth).
  5. **Serialize + persist** — `pczt.serialize()`; compute txid by extracting once.
- Add `update_proof`/`refresh` (D2): load a stored PCZT, `update_ironwood_with` (+ Orchard updater) to set a
  fresh bucketed anchor/witness, re-prove, re-sign, re-serialize.
- Add `extract_broadcast_tx(pczt_bytes) -> Vec<u8>` using the tx-extractor (`extract() -> Transaction`).
- `pool_balances`, `target_and_anchor`, `consensus_network`, `account_uuid`, `parse_usk`, `open_wallet`
  keep their roles; `account_uuid` may store the `AccountUuid` directly (§4.2). The orchestrators
  `sign_schedule` / `sign_split` are rewritten to produce serialized PCZTs.

### `context.rs` (facade)
- `MigrationContext::new` keeps a 16-byte account id at the FFI boundary (FFI-friendly), wrapping to
  `AccountUuid` immediately (§4.2).
- `PreparedTx` returned by `next_due_transfer` / `sign_note_split` now carries `raw_pczt`.
- Method signatures adopt `Zatoshis` / `BlockHeight` where they cross the boundary (e.g. progress, proposals).
- Add a facade method exposing D2 (e.g. `refresh_stale_transfers(&self) -> Result<u32, MigrationError>`),
  and `extract_broadcast_tx`. `record_transfer_result`, scheduling, phase machine otherwise unchanged.

### `scheduling.rs`
- `TransferProposal` construction switches to `Zatoshis`/`BlockHeight`. The per-transfer `amount` is the
  **crossing** value (`D_i`); the spent note is `D_i + BUFFER`. Constants unchanged (288 cadence).

---

## Public API delta (before → after)

| Item | Before | After |
|---|---|---|
| Network type | crate `Network{Main,Test}` | `zcash_protocol::consensus::Network` |
| `TransferProposal.amount` | `amount_zatoshi: u64` | `amount: Zatoshis` |
| heights | `u32` | `BlockHeight` |
| `PreparedTx` payload | `raw_tx: Vec<u8>` (signed tx) | `raw_pczt: Vec<u8>` (serialized PCZT) |
| `MigrationError` | string variants, serde | rich variants (`rusqlite::Error`, `SqliteClientError`, `InvalidStateError`), `Display` + `error_code` |
| SQLite tables | `ironwood_migration_*` | `ext_ironwood_migration_*` |
| denominations | power-of-ten, dust→fee | `power_of_ten + 20_000`, dust→Orchard |
| new ops | — | `refresh_stale_transfers`, `extract_broadcast_tx` |

---

## TDD plan (pure core gets failing-test-first)

1. **denominations.rs** — write failing tests for: each output = `D_i + BUFFER`; `crossing_values` = `D_i`;
   any residual → `orchard_change` (incl. dust); buffer funded; 64-note cap. Then implement.
2. **types.rs** — failing round-trip tests for `Zatoshis`/`BlockHeight` serde adapters and `raw_pczt`. Then
   implement the adapters/renames.
3. **error.rs** — failing tests for `error_code`, `Display`, `From` conversions, `InvalidStateError`. Then
   implement.
4. **store.rs** — update the 14 tests to the `ext_`/`raw_pczt` names (red → green via rename).
5. **scheduling.rs** — adjust tests to `Zatoshis`/`BlockHeight` + crossing-vs-note value.
6. **backend.rs / context.rs (PCZT pipeline)** — **compile-verified** against the real fork (no seeded
   wallet fixture per D-verification). Keep the existing 1–2 backend unit tests that don't need a synced DB.

## Verification

- `cargo test --no-default-features` (pure core: denominations, scheduling, state, store, types, error) — all green.
- `cargo test` (backend feature) — compiles warning-free against the valargroup fork under
  `zcash_unstable="nu6.3"`; core tests green; PCZT pipeline compile-verified.
- `cargo fmt --check` clean.

## Out of scope — recorded in the FINAL REPORT

- **§2.1 gap list** — the valargroup-vs-`feat/ironwood` functionality gap (already produced) is included as
  a report section / `docs/UPSTREAM-GAP-ANALYSIS.md`.
- **§2.3** move off the valargroup forks for release; **§3.3** which pieces are candidates to upstream into
  `zcash_client_backend` (with the maintainership note).
- **§6.1** scanner Reference-retention at `height % 288 == 0` (Orchard + Sapling) and witness-construction
  retry — platform/librustzcash scanner work; precise guidance documented.

## Risks / known wrinkles (to handle or document during implementation)

- **Serde on `Zatoshis`/`BlockHeight`** — confirmed plain newtypes; adapter modules are the plan (D4).
- **Orchard `ProvingKey` cost** — `ProvingKey::build()` is in-memory but ~seconds; build once and cache.
- **Signed-PCZT extraction for broadcast** — the platform (librustzcash-based) extracts; crate provides a
  convenience extractor (D1).
- **Anchor staleness across the schedule** — handled by D2 (`refresh_stale_transfers`), not by deferring proving.
