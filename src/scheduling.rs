//! Height-based anchor bucketing and migration-transfer scheduling (the piece vizor does
//! not provide; vizor de-correlates by time, the app contract is height-based).
//!
//! All transfers in a schedule share one **bucketed anchor** (`floor(natural_anchor / 288) *
//! 288`), which is network-wide for the ~6-hour window — this hides the wallet's last sync
//! time. De-correlation between a wallet's own transfers comes from staggered send heights
//! (`next_executable_after_height`, one bucket apart) and distinct expiry heights. The first
//! privacy-path transfer is delayed one bucket so it does not correlate with the moment the
//! user confirmed the schedule. See the design spec §6.

use crate::types::{MigrationSchedule, TransferProposal};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;

/// Width of an anchor bucket, in blocks (~6 hours at ~75 s/block).
pub(crate) const ANCHOR_BUCKET_SIZE: u32 = 288;
/// Blocks between successive transfers' send windows.
pub(crate) const TRANSFER_CADENCE_BLOCKS: u32 = 288;
/// Delay (blocks) before the first privacy-path transfer may broadcast.
pub(crate) const FIRST_TRANSFER_DELAY_BLOCKS: u32 = 288;
/// Blocks after its send window during which a transfer remains valid.
pub(crate) const TRANSFER_EXPIRY_WINDOW_BLOCKS: u32 = 288;
/// Approximate blocks per hour (~75 s/block).
pub(crate) const BLOCKS_PER_HOUR: u32 = 48;

/// Floor a natural anchor height to its shared network-wide bucket. The result is always
/// `<= natural_anchor` (an anchor in the past, so the note is witnessable now).
pub(crate) fn bucket_anchor(natural_anchor: u32) -> u32 {
    (natural_anchor / ANCHOR_BUCKET_SIZE) * ANCHOR_BUCKET_SIZE
}

/// Build a migration schedule mapping each output `amount` (zatoshi) to a `TransferProposal`.
///
/// All transfers share `bucket_anchor(natural_anchor)`. Transfer `i` may broadcast at
/// `target_height + first_delay_blocks + i * TRANSFER_CADENCE_BLOCKS` and expires
/// `TRANSFER_EXPIRY_WINDOW_BLOCKS` later. Pass `first_delay_blocks = FIRST_TRANSFER_DELAY_BLOCKS`
/// for the privacy path, or `0` for an immediate single transfer.
pub(crate) fn build_schedule(
    run_id: &str,
    amounts: &[u64],
    target_height: u32,
    natural_anchor: u32,
    first_delay_blocks: u32,
) -> MigrationSchedule {
    let anchor_height = BlockHeight::from_u32(bucket_anchor(natural_anchor));
    let transfers = amounts
        .iter()
        .enumerate()
        .map(|(i, &amount)| {
            let next = target_height
                .saturating_add(first_delay_blocks)
                .saturating_add((i as u32).saturating_mul(TRANSFER_CADENCE_BLOCKS));
            TransferProposal {
                id: format!("{run_id}-{i}"),
                amount: Zatoshis::const_from_u64(amount),
                anchor_height,
                next_executable_after_height: BlockHeight::from_u32(next),
                expiry_height: BlockHeight::from_u32(
                    next.saturating_add(TRANSFER_EXPIRY_WINDOW_BLOCKS),
                ),
            }
        })
        .collect();

    MigrationSchedule {
        transfers,
        estimated_duration_hours: estimated_duration_hours(amounts.len(), first_delay_blocks),
    }
}

/// Hours until the last transfer's send window, rounded up. Zero for an empty schedule.
fn estimated_duration_hours(transfer_count: usize, first_delay_blocks: u32) -> u32 {
    let Some(last_index) = transfer_count.checked_sub(1) else {
        return 0;
    };
    let span_blocks = first_delay_blocks
        .saturating_add((last_index as u32).saturating_mul(TRANSFER_CADENCE_BLOCKS));
    span_blocks.div_ceil(BLOCKS_PER_HOUR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_anchor_floors_to_multiple_of_288() {
        assert_eq!(bucket_anchor(2_880_000), 2_880_000); // exact multiple
        assert_eq!(bucket_anchor(2_880_287), 2_880_000); // just below the next bucket
        assert_eq!(bucket_anchor(2_880_288), 2_880_288); // exactly the next bucket
        assert_eq!(bucket_anchor(100), 0);
        assert_eq!(bucket_anchor(0), 0);
    }

    #[test]
    fn bucket_anchor_never_exceeds_input_and_is_aligned() {
        for h in [1u32, 287, 288, 289, 1_000_000, 2_500_001] {
            assert!(bucket_anchor(h) <= h);
            assert_eq!(bucket_anchor(h) % ANCHOR_BUCKET_SIZE, 0);
        }
    }

    #[test]
    fn schedule_is_empty_for_no_amounts() {
        let s = build_schedule("run", &[], 1000, 2000, FIRST_TRANSFER_DELAY_BLOCKS);
        assert!(s.transfers.is_empty());
        assert_eq!(s.estimated_duration_hours, 0);
    }

    #[test]
    fn schedule_shares_one_bucketed_anchor_across_transfers() {
        let s = build_schedule(
            "run",
            &[10, 20, 30],
            1000,
            2_880_290,
            FIRST_TRANSFER_DELAY_BLOCKS,
        );
        let anchors: Vec<u32> = s
            .transfers
            .iter()
            .map(|t| u32::from(t.anchor_height))
            .collect();
        assert_eq!(anchors, vec![2_880_288, 2_880_288, 2_880_288]);
    }

    #[test]
    fn schedule_staggers_send_and_expiry_heights() {
        let s = build_schedule(
            "run",
            &[10, 20, 30],
            1000,
            2000,
            FIRST_TRANSFER_DELAY_BLOCKS,
        );
        let sends: Vec<u32> = s
            .transfers
            .iter()
            .map(|t| u32::from(t.next_executable_after_height))
            .collect();
        assert_eq!(sends, vec![1000 + 288, 1000 + 576, 1000 + 864]);
        let expiries: Vec<u32> = s
            .transfers
            .iter()
            .map(|t| u32::from(t.expiry_height))
            .collect();
        assert_eq!(expiries, vec![1576, 1864, 2152]); // each send + 288
    }

    #[test]
    fn schedule_maps_amounts_in_order() {
        let s = build_schedule(
            "run",
            &[10, 20, 30],
            1000,
            2000,
            FIRST_TRANSFER_DELAY_BLOCKS,
        );
        let amounts: Vec<u64> = s.transfers.iter().map(|t| u64::from(t.amount)).collect();
        assert_eq!(amounts, vec![10, 20, 30]);
    }

    #[test]
    fn schedule_transfer_ids_are_unique_and_carry_run_id() {
        let s = build_schedule(
            "RUN42",
            &[10, 20, 30],
            1000,
            2000,
            FIRST_TRANSFER_DELAY_BLOCKS,
        );
        let ids: Vec<String> = s.transfers.iter().map(|t| t.id.clone()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), 3);
        assert!(ids.iter().all(|id| id.contains("RUN42")));
    }

    #[test]
    fn estimated_duration_matches_app_examples() {
        let three = build_schedule("r", &[1, 2, 3], 1000, 2000, FIRST_TRANSFER_DELAY_BLOCKS);
        assert_eq!(three.estimated_duration_hours, 18);
        let one = build_schedule("r", &[1], 1000, 2000, FIRST_TRANSFER_DELAY_BLOCKS);
        assert_eq!(one.estimated_duration_hours, 6);
    }

    #[test]
    fn immediate_schedule_has_no_first_delay() {
        let s = build_schedule("r", &[1], 1000, 2000, 0);
        assert_eq!(u32::from(s.transfers[0].next_executable_after_height), 1000);
        assert_eq!(s.estimated_duration_hours, 0);
    }
}
