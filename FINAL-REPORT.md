# Final Report — `zodl_ironwood_migration`

Built from the empty repo per `IRONWOOD-MIGRATION-CRATE-HANDOFF.md`, brainstormed spec at
[docs/superpowers/specs/2026-06-30-zodl-ironwood-migration-crate-design.md](docs/superpowers/specs/2026-06-30-zodl-ironwood-migration-crate-design.md),
implemented TDD-first, committed directly to `main` (13 commits).

## Status: complete and verified

- **Builds warning-free** against the real valargroup librustzcash fork under `zcash_unstable="nu6.3"`.
- **57 unit tests pass** with the backend on; **51 pass** core-only (`--no-default-features`).
- `cargo fmt --check` clean.

## What was built

A pure-Rust, synchronous, network-free migration engine the iOS/Android SDKs wrap. Two layers split
by the `librustzcash-backend` Cargo feature:

- **Core (valargroup-free, fully TDD'd):** `plan_denominations` (power-of-10 split, vizor tests
  ported), height-based anchor bucketing + schedule builder, the 14-phase→6-state machine, the
  `ironwood_migration_*` SQLite persistence layer, the public `serde` types, and `MigrationError`.
- **Backend (compile-verified against real APIs):** `ReservedInputSource` (note-reservation
  adapter), balance/height reads, the §8 bucketed-anchor `propose_transaction`(V6) → software
  `create_proposed_transactions` sign flow (no-op Sapling provers), and the `MigrationContext`
  facade that ties it all together and maps 1:1 to the Kotlin `OrchardMigrationSdk`.

## Decisions realized

- **D1 `nu6.3` (corrected the handoff):** the proven `qleak-pr44` branch gates Ironwood on
  `zcash_unstable="nu6.3"` (227 hits) not `nu7` (1). Verified three ways (API gates, vizor `send.rs`,
  vizor's `Cargo.toml` `check-cfg`). `nu7` would have compiled out every Ironwood API.
- **D2 crate prepares+signs+persists, platform broadcasts** — sync, FFI-trivial.
- **D3 vizor power-of-10 split**, **D5 own SQLite tables**, **D6 software-only signing**,
  **D7 pin by rev `0c3ad735`** (orchard by branch, to unify with the workspace).

## Non-blocking findings / documented gaps

1. **One integration gap blocks live signing execution:** building the ZIP-321 *self-address*
   request (paying the account's own unified address) against a live wallet. It is isolated in
   `backend::self_payment_request` (returns a clear error); everything around it — propose at the
   bucketed anchor, force V6, sign with the USK→`SpendingKeys`+no-op provers, serialize, persist —
   is implemented and compile-verified. Completing it needs `WalletRead::get_current_address` (or
   the high-level `create_orchard_to_ironwood_transaction` for the immediate path) and a `Payment`.
2. **Backend exercised only by compilation.** `propose/sign/balance/heights` are verified to compile
   against the real APIs but require a **seeded, synced wallet DB** to run end-to-end (per spec D4 we
   did not build that fixture). The pure core + the SQLite store are fully unit-tested.
3. **Anchor checkpoint retention (§8, security-sensitive):** the bucketed anchor is
   `floor(natural_anchor/288)*288` (≤ natural, ≤287 blocks back). Design is in place; confirming
   `zcash_client_sqlite` retains a checkpoint that deep (and the fallback) needs a synced wallet —
   verify before production, get the privacy design reviewed.
4. **Split algorithm diverges from the app docs' "randomised ~10 notes / 20k cap":** we ship vizor's
   proven deterministic power-of-10 (e.g. 12.345 ZEC → [10,1,1,0.345], capped at 64 notes). Privacy
   comes from the staggered schedule + shared bucketed anchor, not random sizes.
5. **Source-doc contradiction resolved (privacy review item):** Path A §2 ("transfers may share an
   anchor, de-correlate by send height") vs §4.5 ("distinct anchor heights"). We follow §2 + the
   Kotlin doc: all transfers in a schedule share one network-wide bucket anchor; de-correlation is by
   staggered `next_executable_after_height` + distinct expiry. First transfer delayed one bucket so
   it doesn't correlate with the confirm moment.
6. **`is_sync_required_before_next_transfer` returns `false`** — with clean power-of-10 denominations
   each transfer spends a whole pre-split note and produces no Orchard change. Richer change-back
   detection is a future refinement.
7. **Fee handling is a constant estimate** (`FEE_ESTIMATE_ZATOSHI = 10_000`, one ZIP-317 action) for
   planning; actual fees come from the change strategy at sign time.
8. **`store::run_by_id`** is tested but not yet wired into the facade (`active_run` is used); retained
   for future reconciliation code (`#[allow(dead_code)]` with a note).

## For the SDK integrators

- Add this crate as a dep and mirror the `[patch]`/rev pins; set `--cfg zcash_unstable="nu6.3"`.
- Implement the native broadcast: call `next_due_transfer()` → broadcast `raw_tx` via your LWD/Tor
  client honoring `NetworkPrivacyOptions` → `record_transfer_result(id, result)`. The
  `executeNextPendingTransfer` / `submitNoteSplit` Kotlin methods compose from these (see spec §4.3).
- Close gap #1 to enable live signing; then a wallet-DB fixture unlocks end-to-end tests for the
  backend tier.

---

# Addendum — Issue #1 changes (branch `feat/issue-1-upstream-alignment`)

Implements the review notes in [Chlup/ZODLIronwoodMigrationRust#1](https://github.com/Chlup/ZODLIronwoodMigrationRust/issues/1).
Spec: [docs/superpowers/specs/2026-06-30-issue-1-upstream-alignment-pczt-design.md](docs/superpowers/specs/2026-06-30-issue-1-upstream-alignment-pczt-design.md);
plan: [docs/superpowers/plans/2026-06-30-issue-1-upstream-alignment-pczt.md](docs/superpowers/plans/2026-06-30-issue-1-upstream-alignment-pczt.md).

## Status: complete and verified

- **Builds warning-free** against the valargroup fork under `zcash_unstable="nu6.3"`.
- **59 tests pass** with the backend; **53** core-only (`--no-default-features`). `cargo fmt --check` clean.

## What changed

- **PCZT pivot (§2.2/§3.1/§4.1/§4.2):** migration txs are now built, persisted, and refreshed as
  **PCZTs**. `backend::build_signed_pczt` runs propose →
  `create_pczt_from_proposal_with_tx_version(V6)` → prove (Orchard + Ironwood proving keys) → sign
  every Orchard spend (the fork's try-all-indices, qleak-safe pattern) → spend-finalize → serialize.
  `PreparedTx.raw_tx → raw_pczt`. New `extract_broadcast_tx` (PCZT → broadcast bytes) and
  `MigrationContext::refresh_stale_transfers` (the §4.2 re-anchor/re-prove/re-sign "update proof" op).
- **Canonical types (§3.2/§4.1/§4.2):** public API uses `zcash_protocol::consensus::Network`,
  `Zatoshis`, `BlockHeight` (serialized as plain numbers via serde adapters); `MigrationError` is now
  rich (`Db(rusqlite::Error)`, `Backend(SqliteClientError)`, `Pipeline(String)`,
  `InvalidState(InvalidStateError)`) with `error_code()` for the FFI; `zcash_protocol` is a required
  dependency.
- **`ext_` SQLite schema (§3.4):** tables renamed `ironwood_migration_* → ext_ironwood_migration_*`.
- **Self-funding denominations (§5):** each prepared note = `power_of_ten + 20_000` (4× ZIP-317
  marginal fee); any residual/dust is kept in Orchard, never folded into the fee.
- The §2.1 gap list lives in [docs/UPSTREAM-GAP-ANALYSIS.md](docs/UPSTREAM-GAP-ANALYSIS.md).

## Non-blocking findings (out of scope per the approved spec)

1. **§2.3 — move off the valargroup forks (release requirement).** Not done here (this crate only).
   The crate's own code ports cheaply (identical dep versions, same API paths, nu6.3 works upstream);
   the blocker is the wallet-level Ironwood stack upstream still stubs (`ironwood_bundle: None`). See
   the gap-analysis doc. Tracked as the release gate.
2. **§3.3 — push functionality into `zcash_client_backend` (maintainership).** Candidates to upstream
   rather than carry in-crate: the Ironwood balance read and the migration-tx construction. Recorded,
   not implemented (this-crate-only scope).
3. **§6.1 — scanner work (platform/librustzcash).** Set `Reference` checkpoint retention for every
   `height % 288 == 0` block (Orchard **and** Sapling) so a bucketed anchor (≤287 blocks back) is
   always witnessable; and add a witness-construction retry for when a rescan overwrites a `Reference`
   leaf. This crate is scan-free, so these belong in the SDK's sync path / `zcash_client_sqlite`
   scanner. Security-sensitive — get the anchor-retention design reviewed.
4. **Backend tier is compile-verified, not run** (spec D-verification). The PCZT pipeline compiles
   against the real APIs; exercising it needs a seeded, synced wallet DB. Specific runtime items to
   confirm: the **proving-key circuit-version pairing** (`OrchardPostNu6_3` vs `…PreNu6_3` for the
   spend bundle) and that **no `sign_ironwood` pass is needed** (migration spends are Orchard V2; the
   Ironwood bundle is output-only).
5. **`refresh_stale_transfers` regenerates** (re-propose at a fresh bucketed anchor + re-sign) rather
   than mutating a persisted PCZT's anchor in place via the updater role — a future optimization.
6. **Proving on mobile.** Building the Orchard + Ironwood `ProvingKey`s (cached in `OnceLock`) is
   in-memory but non-trivial; if it proves too heavy on device, signing can move to the platform /
   Keystone (the persisted-PCZT model already supports external signing).
7. **`NoteSplitProposal` keeps `u64`/`Vec<u64>`** (not `Zatoshis`) — the issue scoped the canonical
   types to `TransferProposal`/`MigrationProgress`; easy to extend later.

## For the SDK integrators (updated)

- `next_due_transfer()` now returns `PreparedTx { raw_pczt }`. Extract the consensus tx via
  `extract_broadcast_tx(raw_pczt)` (or your own librustzcash binding), broadcast it, then
  `record_transfer_result(id, result)`. Persisted PCZTs also enable Keystone signing and proof/anchor
  refresh (`refresh_stale_transfers`).
