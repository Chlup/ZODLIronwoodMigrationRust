// Ported from vizor-wallet `rust/src/wallet/sync/send.rs`
// (origin/adam/qleak-pr73-orchard-librustzcash), © Chainapsis, Apache-2.0.

//! `ReservedInputSource`: an [`InputSource`] adapter over `zcash_client_sqlite::WalletDb` that
//! excludes notes reserved in the current batch and migration-locked notes. This is the portable
//! note-reservation technique that lets each migration transfer be proposed against a distinct
//! pre-split note. Generic over the inner `InputSource` so it need not name `WalletDb`'s params.

use std::collections::BTreeSet;

use zcash_client_backend::data_api::wallet::{ConfirmationsPolicy, TargetHeight};
use zcash_client_backend::data_api::{
    AccountMeta, InputSource, NoteFilter, ReceivedNotes, TargetValue, TransparentOutputFilter,
};
use zcash_client_backend::wallet::{Note, ReceivedNote, WalletTransparentOutput};
use zcash_protocol::{ShieldedProtocol, TxId};
use zcash_transparent::address::TransparentAddress;
use zcash_transparent::bundle::OutPoint;

/// Merge a caller-supplied exclude list with the reserved set (sorted, de-duplicated).
fn merge_excludes<T: Ord + Copy>(exclude: &[T], reserved: &BTreeSet<T>) -> Vec<T> {
    let mut merged = exclude.to_vec();
    merged.extend(reserved.iter().copied());
    merged.sort_unstable();
    merged.dedup();
    merged
}

/// Whether a note (identified by its txid display string and output index) is migration-locked.
/// The key is the lowercased txid string paired with the output index, matching the store.
fn is_locked(locks: &BTreeSet<(String, u32)>, txid_display: &str, output_index: u32) -> bool {
    locks.contains(&(txid_display.to_lowercase(), output_index))
}

/// An [`InputSource`] adapter that excludes reserved and migration-locked notes.
pub(crate) struct ReservedInputSource<'a, DbT: InputSource> {
    pub inner: &'a DbT,
    pub reserved: &'a BTreeSet<DbT::NoteRef>,
    pub migration_locks: &'a BTreeSet<(String, u32)>,
}

impl<DbT: InputSource> ReservedInputSource<'_, DbT> {
    fn merged_excludes(&self, exclude: &[DbT::NoteRef]) -> Vec<DbT::NoteRef> {
        merge_excludes(exclude, self.reserved)
    }

    fn note_is_locked<N>(&self, note: &ReceivedNote<DbT::NoteRef, N>) -> bool {
        is_locked(
            self.migration_locks,
            &format!("{}", note.txid()),
            note.output_index() as u32,
        )
    }
}

impl<DbT: InputSource> InputSource for ReservedInputSource<'_, DbT> {
    type Error = DbT::Error;
    type AccountId = DbT::AccountId;
    type NoteRef = DbT::NoteRef;

    fn get_spendable_note(
        &self,
        txid: &TxId,
        protocol: ShieldedProtocol,
        index: u32,
        target_height: TargetHeight,
    ) -> Result<Option<ReceivedNote<Self::NoteRef, Note>>, Self::Error> {
        Ok(self
            .inner
            .get_spendable_note(txid, protocol, index, target_height)?
            .filter(|note| !self.reserved.contains(note.internal_note_id()))
            .filter(|note| !self.note_is_locked(note)))
    }

    fn select_spendable_notes(
        &self,
        account: Self::AccountId,
        target_value: TargetValue,
        sources: &[ShieldedProtocol],
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        exclude: &[Self::NoteRef],
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        let selected = self.inner.select_spendable_notes(
            account,
            target_value,
            sources,
            target_height,
            confirmations_policy,
            &self.merged_excludes(exclude),
        )?;
        Ok(ReceivedNotes::new(
            selected.sapling().to_vec(),
            selected
                .orchard()
                .iter()
                .filter(|note| !self.note_is_locked(note))
                .cloned()
                .collect(),
        ))
    }

    fn select_unspent_notes(
        &self,
        account: Self::AccountId,
        sources: &[ShieldedProtocol],
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        let selected =
            self.inner
                .select_unspent_notes(account, sources, target_height, &self.merged_excludes(exclude))?;
        Ok(ReceivedNotes::new(
            selected.sapling().to_vec(),
            selected
                .orchard()
                .iter()
                .filter(|note| !self.note_is_locked(note))
                .cloned()
                .collect(),
        ))
    }

    fn get_account_metadata(
        &self,
        account: Self::AccountId,
        selector: &NoteFilter,
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
    ) -> Result<AccountMeta, Self::Error> {
        self.inner.get_account_metadata(
            account,
            selector,
            target_height,
            &self.merged_excludes(exclude),
        )
    }

    fn get_unspent_transparent_output(
        &self,
        outpoint: &OutPoint,
        target_height: TargetHeight,
    ) -> Result<Option<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        self.inner
            .get_unspent_transparent_output(outpoint, target_height)
    }

    fn get_spendable_transparent_outputs(
        &self,
        address: &TransparentAddress,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        output_filter: TransparentOutputFilter,
    ) -> Result<Vec<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        self.inner.get_spendable_transparent_outputs(
            address,
            target_height,
            confirmations_policy,
            output_filter,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_excludes_unions_sorts_dedups() {
        let reserved: BTreeSet<u32> = [3, 1].into_iter().collect();
        assert_eq!(merge_excludes(&[2, 1, 2], &reserved), vec![1, 2, 3]);
    }

    #[test]
    fn merge_excludes_with_empty_reserved_is_sorted_dedup_excludes() {
        let reserved: BTreeSet<u32> = BTreeSet::new();
        assert_eq!(merge_excludes(&[5, 5, 4], &reserved), vec![4, 5]);
    }

    #[test]
    fn is_locked_matches_lowercased_txid_and_index() {
        let mut locks = BTreeSet::new();
        locks.insert(("aabb".to_string(), 0u32));
        assert!(is_locked(&locks, "AABB", 0));
        assert!(is_locked(&locks, "aabb", 0));
        assert!(!is_locked(&locks, "AABB", 1));
        assert!(!is_locked(&locks, "CCDD", 0));
    }
}
