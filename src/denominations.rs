// Ported from vizor-wallet `rust/src/wallet/sync/migration.rs`
// (origin/adam/qleak-pr73-orchard-librustzcash), © Chainapsis, Apache-2.0.

//! Note-split planning: deterministic power-of-10 decomposition (ported from vizor's
//! `plan_denominations`). Pure arithmetic, fully unit-tested.

pub(crate) const ZATOSHIS_PER_ZEC: u64 = 100_000_000;
pub(crate) const MIGRATION_MAX_PREPARED_NOTES_PER_RUN: usize = 64;
/// The smallest residual kept as a migration output (below this it folds into the fee).
pub(crate) const MIN_IRONWOOD_MIGRATION_OUTPUT_ZATOSHI: u64 = 1;

/// The outcome of planning a note split: the per-note migration output values, an optional
/// residual kept in Orchard, and the fee/accounting totals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DenominationPlan {
    pub migration_outputs: Vec<u64>,
    pub orchard_change: Option<u64>,
    pub prep_fee_zatoshi: u64,
    pub migration_fee_zatoshi: u64,
    pub total_input_zatoshi: u64,
    pub total_migratable_zatoshi: u64,
}

/// Decompose `total_input_zatoshi` (after reserving the prep fee) into a set of
/// power-of-10 ZEC denominations plus a sub-ZEC residual, capped at
/// [`MIGRATION_MAX_PREPARED_NOTES_PER_RUN`] notes.
pub(crate) fn plan_denominations(
    total_input_zatoshi: u64,
    prep_fee_zatoshi: u64,
    migration_fee_zatoshi: u64,
    minimum_output_zatoshi: u64,
) -> Result<DenominationPlan, String> {
    if total_input_zatoshi <= prep_fee_zatoshi {
        return Ok(DenominationPlan {
            migration_outputs: Vec::new(),
            orchard_change: None,
            prep_fee_zatoshi: total_input_zatoshi,
            migration_fee_zatoshi,
            total_input_zatoshi,
            total_migratable_zatoshi: 0,
        });
    }

    let available = total_input_zatoshi
        .checked_sub(prep_fee_zatoshi)
        .ok_or("Denomination prep fee underflow")?;
    let whole_zec = available / ZATOSHIS_PER_ZEC;
    let remainder = available % ZATOSHIS_PER_ZEC;
    let mut outputs = Vec::new();

    let mut denom = 1u64;
    while denom <= whole_zec / 10 {
        denom = denom.checked_mul(10).ok_or("Denomination overflow")?;
    }

    let mut remaining_whole = whole_zec;
    while denom > 0 {
        while remaining_whole >= denom {
            outputs.push(
                denom
                    .checked_mul(ZATOSHIS_PER_ZEC)
                    .ok_or("Denomination zatoshi overflow")?,
            );
            remaining_whole -= denom;
        }
        denom /= 10;
    }

    let migratable_residual_threshold = migration_fee_zatoshi
        .checked_add(minimum_output_zatoshi)
        .ok_or("Residual fee threshold overflow")?;
    let orchard_change = if remainder > migratable_residual_threshold {
        outputs.push(remainder);
        None
    } else if remainder >= minimum_output_zatoshi {
        Some(remainder)
    } else {
        // A residual below the minimum output intentionally becomes extra transaction fee.
        None
    };

    if outputs.len() > MIGRATION_MAX_PREPARED_NOTES_PER_RUN {
        return Err(format!(
            "Migration plan would create {} prepared notes, above the {} note limit",
            outputs.len(),
            MIGRATION_MAX_PREPARED_NOTES_PER_RUN
        ));
    }

    let total_migratable_zatoshi = outputs.iter().try_fold(0u64, |acc, value| {
        acc.checked_add(*value)
            .ok_or("Migratable total overflow".to_string())
    })?;

    Ok(DenominationPlan {
        migration_outputs: outputs,
        orchard_change,
        prep_fee_zatoshi,
        migration_fee_zatoshi,
        total_input_zatoshi,
        total_migratable_zatoshi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMUM_OUTPUT_FOR_TEST: u64 = 1;

    #[test]
    fn planner_noops_when_prep_fee_consumes_balance() {
        let plan = plan_denominations(5_000, 10_000, 10_000, 1).unwrap();

        assert!(plan.migration_outputs.is_empty());
        assert_eq!(plan.total_migratable_zatoshi, 0);
        assert_eq!(plan.prep_fee_zatoshi, 5_000);
    }

    #[test]
    fn planner_creates_decimal_denominations_and_fee_positive_residual() {
        let plan = plan_denominations(1_234_500_000, 0, 10_000, MINIMUM_OUTPUT_FOR_TEST).unwrap();

        assert_eq!(
            plan.migration_outputs,
            vec![1_000_000_000, 100_000_000, 100_000_000, 34_500_000]
        );
        assert_eq!(plan.orchard_change, None);
        assert_eq!(plan.total_migratable_zatoshi, 1_234_500_000);
    }

    #[test]
    fn planner_keeps_non_fee_positive_residual_as_orchard_change() {
        let plan = plan_denominations(100_010_000, 0, 10_000, MINIMUM_OUTPUT_FOR_TEST).unwrap();

        assert_eq!(plan.migration_outputs, vec![100_000_000]);
        assert_eq!(plan.orchard_change, Some(10_000));
    }

    #[test]
    fn planner_reserves_prep_fee_before_decomposition() {
        let plan = plan_denominations(1_000_000_000, 10_000, 10_000, 1).unwrap();

        assert_eq!(
            plan.migration_outputs,
            vec![
                100_000_000,
                100_000_000,
                100_000_000,
                100_000_000,
                100_000_000,
                100_000_000,
                100_000_000,
                100_000_000,
                100_000_000,
                99_990_000,
            ]
        );
    }

    #[test]
    fn planner_rejects_more_than_max_prepared_outputs() {
        let err = plan_denominations(1_999_999_950_000_000, 0, 10_000, 1).unwrap_err();

        assert!(err.contains("above the 64 note limit"));
    }
}
