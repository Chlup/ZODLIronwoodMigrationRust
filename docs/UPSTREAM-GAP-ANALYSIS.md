# Valargroup fork vs upstream `zcash/librustzcash` (`feat/ironwood`) — Ironwood gap analysis

Answers issue #1 §2 bullet 1: *which functionality in the valargroup forks does not yet exist in
[`zcash/librustzcash@feat/ironwood`](https://github.com/zcash/librustzcash/tree/feat/ironwood)?*
This is the prerequisite for the release requirement (§2 bullet 3) to move off the valargroup forks.

Comparison done by fetching both branches into one object DB and diffing from their merge-base.

## Relationship

| | Valargroup | Upstream |
|---|---|---|
| Branch | `adam/qleak-pr44-orchard-dummy-ciphertexts` | `feat/ironwood` |
| Pinned HEAD at time of writing | `0c3ad735` (2026-06-19) | `676e8dd6` (2026-06-29) |
| orchard pin | branch `adam/qleak-dummy-ciphertexts-on-pr505` (0.14 line) | `0.15.0-pre.0` @ rev `2f322a22` |

They **diverged from a common base on 2026-06-10** and ran in parallel (~76 valargroup-unique vs ~72
upstream-unique commits) — not "fork = upstream + one patch". The shared `adam/ironwood-split-*`
lineage is being upstreamed independently. **Every workspace crate version is identical** across the
two forks (zcash_client_backend 0.23, zcash_primitives 0.28, zcash_keys 0.14, zcash_protocol 0.9,
zip321 0.8, pczt 0.7, …; zcash_client_sqlite 0.21.0 vs 0.21.1) — only the orchard line differs.

## In valargroup, NOT in upstream `feat/ironwood`

Upstream's Ironwood support is real but lives entirely at the **PCZT + transaction-primitives**
layer. The **wallet layer** (`zcash_client_backend` / `zcash_client_sqlite`) is what valargroup adds:

1. **Wallet-level Ironwood balance.** `AccountBalance` gains an `ironwood_balance: Balance` field +
   `ironwood_balance()` accessor (`zcash_client_backend/src/data_api.rs`); `spendable_value()` counts
   V3/Ironwood value only under `zcash_unstable="nu6.3"` (the "V3 spendable policy"). **Upstream has
   no `ironwood_balance` anywhere.**
2. **Wallet-level migration tx builder.** `create_orchard_to_ironwood_transaction(...)` and the
   `proposed_version`→V6 dispatch (`orchard_outputs_to_ironwood` / `legacy_orchard_bundle_requested`)
   that routes Orchard-destined outputs through `add_ironwood_output` → V3 notes
   (`zcash_client_backend/src/data_api/wallet.rs`, +1171). **Upstream forcing V6 changes only the tx
   *format*; its `zcash_primitives` builder hard-codes `ironwood_bundle: None`** ("does not yet
   construct Ironwood bundles").
3. **Core builder Ironwood construction.** `orchard::BundleProtocol::IronwoodPostNu6_3`,
   `add_ironwood_spend` / `add_ironwood_change_output`, the `ironwood_anchor` in `BuildConfig`, gated
   on `BranchId::Nu6_3` (`zcash_primitives/src/transaction/builder.rs`, +1352).
4. **SQLite storage for Ironwood notes.** V3 notes reuse `orchard_received_notes` discriminated by a
   `note_version` column; three migrations (`orchard_note_version_uniqueness`, `ironwood_shardtree`,
   `ironwood_pool_code_views`) add the column + triple-uniqueness key, the `ironwood_tree_*`
   commitment-tree tables + `v_ironwood_*` scan views, and pool-code 4. **Upstream
   `zcash_client_sqlite/src/` has zero Ironwood references** and pins note decryption to V2.
5. **The qleak / Orchard dummy-ciphertext privacy fix** (orchard crate, rev `204d8ce`): the
   spend-paired fabricated zero-value output carries a randomized, undecryptable `enc_ciphertext`, so
   an ivk-holder (quantum-recoverable from the published self-send address) cannot detect the spend.
   Load-bearing for migration privacy (a migration *is* a self-send). Travels with the orchard pin
   only — no librustzcash code. **Upstream's orchard pin (`2f322a22`) does not include it.**

## In upstream `feat/ironwood`, NOT in valargroup's pinned branch

- **PCZT-centric Ironwood**: a dedicated `ironwood` bundle threaded through every PCZT role
  (creator/prover/signer/io_finalizer/combiner/updater/redactor/verifier/tx_extractor),
  `into_ironwood_parsed()`/`ironwood_v3()`, PCZT v2 serialization. *(Note: valargroup's pinned branch
  also carries the PCZT Ironwood roles — this crate's pivot relies on them — but upstream's are newer
  and tied to the newer orchard API.)*
- **ZIP-317 marginal fees for Ironwood actions** (real fee accounting; `ironwood_action_count`
  threaded into the fee rule).
- **Newer Orchard line** (0.15.0-pre.0): BundleVersion API, 1-based versioning, "Lift Flags out of
  BundleType" — a breaking API delta from valargroup's 0.14/PR505 line.
- Maintenance: TZE removed, `zcash_client_memory` removed, delete_account fixes, GHSA fixes.

## cfg

Both repos carry `NetworkUpgrade::Nu6_3` (and `Nu7`) in the consensus enum. Upstream gates Ironwood
V6 under `any(zcash_unstable="nu6.3", zcash_unstable="nu7")` (NU6.3 is canonical; nu7 only adds
ZIP-233). Valargroup's Ironwood code is overwhelmingly `nu6.3`-gated. This crate builds with
`--cfg zcash_unstable="nu6.3"`, which would also light up the V6 primitives upstream.

## Port implications (for §2.3 "move off the valargroup forks")

This crate's own code ports cheaply: dependency **versions are identical**, every non-Ironwood API it
uses resolves at the **same path** upstream, and its `nu6.3` cfg works upstream. The blocker is items
1–4 above — the **wallet-level Ironwood construction + balance + V3 storage** that upstream
deliberately stubbed (`ironwood_bundle: None`). The realistic path (§3 "add functionality to
`zcash_client_backend`", leveraging librustzcash maintainership) is to land that wallet stack
upstream — adapting it to the newer Orchard 0.15 API — plus a qleak-bearing orchard pin. Once those
exist upstream, this crate's switch is a Cargo-source change.
