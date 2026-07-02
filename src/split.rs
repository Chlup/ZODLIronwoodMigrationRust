// Note split via same-address V2 change outputs (design spec
// docs/superpowers/specs/2026-07-02-note-split-same-address-change-design.md, zodl-ios repo).
//
// Post-NU6.3 the `OrchardPostNu6_3` protocol disables cross-address transfers: payment outputs are
// rejected (`CrossAddressDisabled`), but the orchard builder sanctions any number of
// wallet-controlled **change** outputs, each paired with a fabricated zero-value spend at the
// change's own address. The fork's `create_pczt_from_proposal` routes all V6 Orchard-destined
// outputs (payments AND change) to Ironwood, so this module drives the public
// `zcash_primitives::transaction::builder::Builder` directly instead.

use crate::error::MigrationError;

/// ZIP-317 marginal fee per logical action (zatoshi).
const MARGINAL_FEE_ZATOSHI: u64 = 5_000;
/// ZIP-317 grace floor on the action count.
const GRACE_ACTIONS: u64 = 2;

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
