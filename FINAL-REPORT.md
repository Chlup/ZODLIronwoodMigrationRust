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
