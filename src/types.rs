//! Public, `serde`-derivable data types exchanged across the FFI/JNI boundary.
//!
//! Amounts use [`zcash_protocol::value::Zatoshis`] and heights use
//! [`zcash_protocol::consensus::BlockHeight`] — the canonical librustzcash types, which enforce
//! their own invariants. The FFI/JNI glue marshals these as JSON; the `Zatoshis`/`BlockHeight`
//! newtypes are serialized as plain `u64`/`u32` numbers via the adapter modules below.

use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;

/// The Zcash network the wallet operates on (re-exported canonical type).
pub use zcash_protocol::consensus::Network;

/// serde adapter: (de)serialize a [`Zatoshis`] as a plain `u64`.
pub(crate) mod serde_zatoshis {
    use super::Zatoshis;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Zatoshis, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::from(*v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Zatoshis, D::Error> {
        Zatoshis::from_u64(u64::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// serde adapter: (de)serialize a [`BlockHeight`] as a plain `u32`.
pub(crate) mod serde_block_height {
    use super::BlockHeight;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &BlockHeight, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(u32::from(*v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BlockHeight, D::Error> {
        Ok(BlockHeight::from_u32(u32::deserialize(d)?))
    }
}

/// serde adapter: (de)serialize an `Option<BlockHeight>` as an optional plain `u32`.
pub(crate) mod serde_opt_block_height {
    use super::BlockHeight;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<BlockHeight>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(h) => s.serialize_some(&u32::from(*h)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<BlockHeight>, D::Error> {
        Ok(Option::<u32>::deserialize(d)?.map(BlockHeight::from_u32))
    }
}

/// How the platform should broadcast migration transactions.
///
/// `submission_endpoint == None` means broadcast over the same lightwalletd server used for
/// sync; a secondary endpoint de-correlates sync traffic from broadcast traffic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPrivacyOptions {
    pub use_tor: bool,
    pub submission_endpoint: Option<String>,
}

/// A proposed note split: the per-note output values (zatoshi) and the prep-transaction fee.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteSplitProposal {
    pub output_notes: Vec<u64>,
    pub fee: u64,
}

/// A single scheduled migration transfer.
///
/// `amount` is the value that crosses the turnstile — a power-of-ten ZEC amount; the note actually
/// spent is `amount + TRANSFER_FEE_BUFFER_ZATOSHI` (the note pays its own transfer fee).
/// `anchor_height` comes from a shared network-wide 288-block bucket; `next_executable_after_height`
/// is when the platform may broadcast it; after `expiry_height` it is invalid and the step must be
/// restarted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferProposal {
    pub id: String,
    #[serde(with = "serde_zatoshis")]
    pub amount: Zatoshis,
    #[serde(with = "serde_block_height")]
    pub anchor_height: BlockHeight,
    #[serde(with = "serde_block_height")]
    pub next_executable_after_height: BlockHeight,
    #[serde(with = "serde_block_height")]
    pub expiry_height: BlockHeight,
}

/// The full migration schedule presented to the user for one-time confirmation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSchedule {
    pub transfers: Vec<TransferProposal>,
    pub estimated_duration_hours: u32,
}

/// Live migration progress for the progress UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationProgress {
    pub completed_transfers: u32,
    pub total_transfers: u32,
    #[serde(with = "serde_zatoshis")]
    pub remaining_orchard: Zatoshis,
    #[serde(with = "serde_opt_block_height")]
    pub next_transfer_ready_at_height: Option<BlockHeight>,
}

/// A pre-signed transaction for the platform to broadcast, carried as a serialized PCZT.
///
/// `raw_pczt` is the serialized `pczt::Pczt` (proven + signed); `txid` is its (pre-computed)
/// transaction id. The platform extracts the consensus transaction from the PCZT (one librustzcash
/// call) and broadcasts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedTx {
    pub id: String,
    pub txid: String,
    pub raw_pczt: Vec<u8>,
}

/// Top-level migration state machine surfaced to the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationState {
    /// No migration has been initiated.
    NotStarted,
    /// Note-split transaction submitted, awaiting on-chain confirmation.
    SplitPendingConfirmation,
    /// Split confirmed (or not needed); ready to propose transfers.
    ReadyToPropose,
    /// Schedule committed; transfers are executing.
    InProgress(MigrationProgress),
    /// A transfer cannot proceed automatically; the app must act.
    RequiresAttention(AttentionReason),
    /// All transfers confirmed; Orchard balance is migrated.
    Complete,
}

/// Why a migration requires user attention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionReason {
    /// An input note was spent externally before its transfer was broadcast.
    InvalidTransfer { transfer_id: String },
    /// A transaction's anchor/expiry elapsed before broadcast.
    TransferExpired,
    /// A transfer produced change back to Orchard that must be synced before the next spend.
    SyncRequiredBeforeNext,
}

/// The outcome of a broadcast attempt, reported back to the engine by the platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferResult {
    Success {
        txid: String,
    },
    /// Transient network failure; `retryable` indicates whether to retry in a later window.
    NetworkError {
        retryable: bool,
    },
    /// The input note was already spent.
    InvalidNote,
    /// The transaction's anchor/expiry height has passed.
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_protocol::consensus::BlockHeight;
    use zcash_protocol::value::Zatoshis;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn note_split_proposal_round_trips() {
        let p = NoteSplitProposal {
            output_notes: vec![100_000_000, 34_500_000],
            fee: 10_000,
        };
        assert_eq!(round_trip(&p), p);
    }

    #[test]
    fn transfer_proposal_round_trips_canonical_types() {
        let t = TransferProposal {
            id: "abc".to_string(),
            amount: Zatoshis::const_from_u64(1_000_000_000),
            anchor_height: BlockHeight::from_u32(2_880_000),
            next_executable_after_height: BlockHeight::from_u32(2_880_288),
            expiry_height: BlockHeight::from_u32(2_880_576),
        };
        let json = serde_json::to_string(&t).unwrap();
        // Canonical newtypes are marshaled as plain numbers (not nested objects).
        assert!(json.contains("1000000000"));
        assert!(json.contains("2880576"));
        let back: TransferProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(back.amount, t.amount);
        assert_eq!(back.expiry_height, t.expiry_height);
        assert_eq!(back, t);
    }

    #[test]
    fn migration_schedule_round_trips() {
        let s = MigrationSchedule {
            transfers: vec![TransferProposal {
                id: "t1".to_string(),
                amount: Zatoshis::const_from_u64(5),
                anchor_height: BlockHeight::from_u32(1),
                next_executable_after_height: BlockHeight::from_u32(2),
                expiry_height: BlockHeight::from_u32(3),
            }],
            estimated_duration_hours: 6,
        };
        assert_eq!(round_trip(&s), s);
    }

    #[test]
    fn migration_progress_round_trips() {
        let p = MigrationProgress {
            completed_transfers: 2,
            total_transfers: 5,
            remaining_orchard: Zatoshis::const_from_u64(600_000_000),
            next_transfer_ready_at_height: Some(BlockHeight::from_u32(2_880_864)),
        };
        assert_eq!(round_trip(&p), p);
        let none = MigrationProgress {
            next_transfer_ready_at_height: None,
            ..p
        };
        assert_eq!(round_trip(&none), none);
    }

    #[test]
    fn prepared_tx_carries_raw_pczt() {
        let tx = PreparedTx {
            id: "t1".to_string(),
            txid: "deadbeef".to_string(),
            raw_pczt: vec![0x50, 0x00, 0xff],
        };
        assert_eq!(round_trip(&tx), tx);
    }

    #[test]
    fn migration_state_round_trips_each_variant() {
        let progress = MigrationProgress {
            completed_transfers: 1,
            total_transfers: 3,
            remaining_orchard: Zatoshis::const_from_u64(1),
            next_transfer_ready_at_height: None,
        };
        for state in [
            MigrationState::NotStarted,
            MigrationState::SplitPendingConfirmation,
            MigrationState::ReadyToPropose,
            MigrationState::InProgress(progress),
            MigrationState::RequiresAttention(AttentionReason::TransferExpired),
            MigrationState::RequiresAttention(AttentionReason::SyncRequiredBeforeNext),
            MigrationState::RequiresAttention(AttentionReason::InvalidTransfer {
                transfer_id: "x".to_string(),
            }),
            MigrationState::Complete,
        ] {
            assert_eq!(round_trip(&state), state);
        }
    }

    #[test]
    fn transfer_result_round_trips_each_variant() {
        for result in [
            TransferResult::Success {
                txid: "abc".to_string(),
            },
            TransferResult::NetworkError { retryable: true },
            TransferResult::InvalidNote,
            TransferResult::Expired,
        ] {
            assert_eq!(round_trip(&result), result);
        }
    }

    #[test]
    fn network_privacy_options_round_trips() {
        let o = NetworkPrivacyOptions {
            use_tor: true,
            submission_endpoint: Some("https://lwd.example:9067".to_string()),
        };
        assert_eq!(round_trip(&o), o);
    }
}
