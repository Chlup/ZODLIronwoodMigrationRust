// Note split via same-address V2 change outputs (design spec
// docs/superpowers/specs/2026-07-02-note-split-same-address-change-design.md, zodl-ios repo).
//
// Post-NU6.3 the `OrchardPostNu6_3` protocol disables cross-address transfers: payment outputs are
// rejected (`CrossAddressDisabled`), but the orchard builder sanctions any number of
// wallet-controlled **change** outputs, each paired with a fabricated zero-value spend at the
// change's own address. The fork's `create_pczt_from_proposal` routes all V6 Orchard-destined
// outputs (payments AND change) to Ironwood, so this module drives the public
// `zcash_primitives::transaction::builder::Builder` directly instead.

use std::collections::BTreeSet;

use rand::rngs::OsRng;
use zcash_client_backend::data_api::wallet::ConfirmationsPolicy;
use zcash_client_backend::data_api::{InputSource, TargetValue, WalletCommitmentTrees, WalletRead};
use zcash_client_backend::wallet::ReceivedNote;
use zcash_client_sqlite::{AccountUuid, ReceivedNoteId};
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::builder::{BuildConfig, Builder};
use zcash_primitives::transaction::fees::zip317::FeeRule as Zip317FeeRule;
use zcash_protocol::consensus::{BlockHeight, Parameters};
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::ShieldedProtocol;

use crate::backend::Db;
use crate::error::MigrationError;
use crate::reserved_source::ReservedInputSource;

/// ZIP-317 marginal fee per logical action (zatoshi).
const MARGINAL_FEE_ZATOSHI: u64 = 5_000;
/// ZIP-317 grace floor on the action count.
const GRACE_ACTIONS: u64 = 2;

/// All spendable Orchard **V2** notes for `account`, excluding migration-locked notes. Selection
/// goes through [`ReservedInputSource`] so its (txid, output_index) lock filtering applies; V3
/// (Ironwood) notes are filtered out defensively — at split time none should exist yet.
pub(crate) fn select_spendable_v2_notes<P: Parameters>(
    db: &Db<P>,
    account: AccountUuid,
    migration_locks: &BTreeSet<(String, u32)>,
) -> Result<Vec<ReceivedNote<ReceivedNoteId, orchard::note::Note>>, MigrationError> {
    let (target, _anchor) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())?
        .ok_or(MigrationError::NotSynced)?;
    let total = crate::backend::pool_balances(db, account)?.orchard_spendable;
    if total == 0 {
        return Err(MigrationError::Pipeline(
            "note split: no spendable Orchard balance".into(),
        ));
    }
    let reserved: BTreeSet<ReceivedNoteId> = BTreeSet::new();
    let source = ReservedInputSource {
        inner: db,
        reserved: &reserved,
        migration_locks,
    };
    let notes = source
        .select_spendable_notes(
            account,
            TargetValue::AtLeast(Zatoshis::const_from_u64(total)),
            &[ShieldedProtocol::Orchard],
            target,
            ConfirmationsPolicy::default(),
            &[],
        )
        .map_err(|e| MigrationError::Pipeline(format!("note split: select notes: {e:?}")))?
        .take_orchard();
    Ok(notes
        .into_iter()
        .filter(|n| n.note().version() == orchard::note::NoteVersion::V2)
        .collect())
}

/// Exact ZIP-317 fee for the split transaction. The bundle disables cross-address transfers, so
/// each spend and each change output occupies its own action: `actions = n_spends + n_changes`
/// (floored at the grace count). No sapling or transparent components exist in a split.
pub(crate) fn split_fee(n_spends: usize, n_changes: usize) -> u64 {
    let actions = (n_spends as u64).saturating_add(n_changes as u64);
    MARGINAL_FEE_ZATOSHI * actions.max(GRACE_ACTIONS)
}

/// Make the planned outputs balance exactly: `Σ(outputs) = selected_total − fee`, with the last
/// output absorbing the residual (the denomination plan was made against an estimated fee and the
/// wallet's balance snapshot; the builder requires an exact balance). Errors when the fee exceeds
/// the selected total, when there are no outputs, or when absorption would make the last output
/// non-positive.
pub(crate) fn adjust_outputs_for_exact_balance(
    selected_total: u64,
    fee: u64,
    outputs: &[u64],
) -> Result<Vec<u64>, MigrationError> {
    let required: u64 = selected_total
        .checked_sub(fee)
        .ok_or_else(|| {
            MigrationError::Pipeline(format!(
                "note split: fee {fee} exceeds selected total {selected_total}"
            ))
        })?;
    let mut adjusted = outputs.to_vec();
    let current: u64 = adjusted.iter().sum();
    let last = adjusted
        .last_mut()
        .ok_or_else(|| MigrationError::Pipeline("note split: no outputs to adjust".into()))?;
    let new_last = (*last as i128) + (required as i128) - (current as i128);
    if new_last <= 0 {
        return Err(MigrationError::Pipeline(format!(
            "note split: residual absorption drives the last output to {new_last} zatoshi"
        )));
    }
    *last = new_last as u64;
    Ok(adjusted)
}

/// Build the note-split transaction as an unproven PCZT: spend every spendable V2 note and fan the
/// value into one same-address change output per planned denomination. Runs entirely on public
/// fork APIs (the fork's `create_pczt_from_proposal` cannot keep V6 Orchard outputs in the V2
/// pool). Returns the PCZT and the residual-adjusted output values actually used.
///
/// The bundle is `OrchardPostNu6_3` (current circuit): `Builder::new` derives the protocol from
/// the target height's consensus branch, and `build_for_pczt` selects `V6` because that builder is
/// in use. Change outputs are sanctioned under the cross-address restriction — the orchard builder
/// pairs each with a fabricated zero-value spend at the change's own address, signed by the normal
/// signing flow with the wallet's spend-authorizing key.
// Wired into sign_split by the follow-up commit; the allow keeps this commit warning-clean.
#[allow(dead_code)]
pub(crate) fn build_split_pczt<P: Parameters>(
    db: &mut Db<P>,
    network: &P,
    account: AccountUuid,
    usk: &UnifiedSpendingKey,
    migration_locks: &BTreeSet<(String, u32)>,
    outputs: &[u64],
) -> Result<(pczt::Pczt, Vec<u64>), MigrationError> {
    // --- immutable phase: select the notes to consolidate ---
    let notes = select_spendable_v2_notes(db, account, migration_locks)?;
    if notes.is_empty() {
        return Err(MigrationError::Pipeline(
            "note split: no spendable Orchard V2 notes".into(),
        ));
    }
    let selected_total: u64 = notes.iter().map(|n| n.note().value().inner()).sum();
    let fee = split_fee(notes.len(), outputs.len());
    let adjusted = adjust_outputs_for_exact_balance(selected_total, fee, outputs)?;

    let (target, natural_anchor) = crate::backend::target_and_anchor(db)?;
    let anchor_height = BlockHeight::from_u32(natural_anchor);

    // --- mutable phase: anchor root + witness per spent note ---
    let (anchor, spends) = db.with_orchard_tree_mut::<_, _, MigrationError>(|tree| {
        let anchor: orchard::Anchor = tree
            .root_at_checkpoint_id(&anchor_height)?
            .ok_or_else(|| {
                MigrationError::Pipeline(format!(
                    "note split: anchor not found at height {anchor_height}"
                ))
            })?
            .into();
        let mut spends: Vec<(orchard::note::Note, orchard::tree::MerklePath)> = Vec::new();
        for received in &notes {
            let merkle_path = tree
                .witness_at_checkpoint_id_caching(
                    received.note_commitment_tree_position(),
                    &anchor_height,
                )?
                .ok_or_else(|| {
                    MigrationError::Pipeline(format!(
                        "note split: witness checkpoint pruned at {anchor_height}"
                    ))
                })?;
            spends.push((*received.note(), merkle_path.into()));
        }
        Ok((anchor, spends))
    })?;

    // --- build: n spends + k same-address change outputs, exact balance ---
    let mut builder = Builder::new(
        network.clone(),
        BlockHeight::from_u32(target),
        BuildConfig::Standard {
            sapling_anchor: None,
            orchard_anchor: Some(anchor),
            ironwood_anchor: None,
        },
    );
    let orchard_fvk = orchard::keys::FullViewingKey::from(usk.orchard());
    for (note, merkle_path) in spends {
        builder
            .add_orchard_spend::<std::convert::Infallible>(orchard_fvk.clone(), note, merkle_path)
            .map_err(|e| MigrationError::Pipeline(format!("note split: add spend: {e:?}")))?;
    }
    let change_address = orchard_fvk.address_at(0u32, orchard::keys::Scope::Internal);
    let internal_ovk = orchard_fvk.to_ovk(orchard::keys::Scope::Internal);
    for value in &adjusted {
        builder
            .add_orchard_change_output::<std::convert::Infallible>(
                orchard_fvk.clone(),
                Some(internal_ovk.clone()),
                change_address,
                Zatoshis::const_from_u64(*value),
                MemoBytes::empty(),
            )
            .map_err(|e| MigrationError::Pipeline(format!("note split: add change: {e:?}")))?;
    }

    let build_result = builder
        .build_for_pczt(OsRng, &Zip317FeeRule::standard())
        .map_err(|e| MigrationError::Pipeline(format!("note split: build: {e:?}")))?;

    // --- assemble the PCZT (Creator → IoFinalizer), mirroring the fork's create_pczt tail ---
    let created = pczt::roles::creator::Creator::build_from_parts(build_result.pczt_parts)
        .ok_or_else(|| MigrationError::Pipeline("note split: pczt creation failed".into()))?;
    let finalized = pczt::roles::io_finalizer::IoFinalizer::new(created)
        .finalize_io()
        .map_err(|e| MigrationError::Pipeline(format!("note split: io finalize: {e:?}")))?;

    Ok((finalized, adjusted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_fee_is_marginal_fee_times_actions() {
        // Cross-address disabled: actions = spends + changes; ZIP-317 marginal fee 5000.
        assert_eq!(split_fee(1, 2), 15_000);
        assert_eq!(split_fee(1, 9), 50_000);
        assert_eq!(split_fee(2, 3), 25_000);
    }

    #[test]
    fn split_fee_applies_the_two_action_grace_floor() {
        assert_eq!(split_fee(1, 0), 10_000);
        assert_eq!(split_fee(0, 1), 10_000);
    }

    #[test]
    fn adjust_keeps_outputs_when_balance_is_exact() {
        let adjusted =
            adjust_outputs_for_exact_balance(1_000_000, 15_000, &[500_000, 485_000]).unwrap();
        assert_eq!(adjusted, vec![500_000, 485_000]);
    }

    #[test]
    fn adjust_absorbs_the_residual_in_the_last_output() {
        // Planned against an estimated fee; the exact fee differs → last output absorbs the delta.
        let adjusted =
            adjust_outputs_for_exact_balance(1_000_000, 15_000, &[500_000, 400_000]).unwrap();
        assert_eq!(adjusted, vec![500_000, 485_000]);
        let adjusted =
            adjust_outputs_for_exact_balance(1_000_000, 15_000, &[500_000, 500_000]).unwrap();
        assert_eq!(adjusted, vec![500_000, 485_000]);
    }

    #[test]
    fn adjust_rejects_a_nonpositive_last_output() {
        assert!(adjust_outputs_for_exact_balance(1_000_000, 15_000, &[985_000, 10_000]).is_err());
    }

    #[test]
    fn adjust_rejects_fee_exceeding_total_and_empty_outputs() {
        assert!(adjust_outputs_for_exact_balance(10_000, 15_000, &[5_000]).is_err());
        assert!(adjust_outputs_for_exact_balance(1_000_000, 15_000, &[]).is_err());
    }
}
