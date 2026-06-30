//! Public, `serde`-derivable data types exchanged across the FFI/JNI boundary.
//!
//! All amounts are zatoshi (`u64`); all block heights are `u32`. The FFI/JNI glue marshals
//! these (JSON today); enums use serde's default external tagging, which the platform glue
//! turns into a tag + fields.

use serde::{Deserialize, Serialize};

/// The Zcash network the wallet operates on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Network {
    Main,
    Test,
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
/// `anchor_height` comes from a shared network-wide 288-block bucket; `next_executable_after_height`
/// is when the platform may broadcast it; after `expiry_height` it is invalid and the step must be
/// restarted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferProposal {
    pub id: String,
    pub amount_zatoshi: u64,
    pub anchor_height: u32,
    pub next_executable_after_height: u32,
    pub expiry_height: u32,
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
    pub remaining_orchard_zatoshi: u64,
    pub next_transfer_ready_at_height: Option<u32>,
}

/// A pre-signed transaction for the platform to broadcast. `raw_tx` is the consensus-encoded
/// transaction; `txid` is its (pre-computed) transaction id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedTx {
    pub id: String,
    pub txid: String,
    pub raw_tx: Vec<u8>,
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
    Success { txid: String },
    /// Transient network failure; `retryable` indicates whether to retry in a later window.
    NetworkError { retryable: bool },
    /// The input note was already spent.
    InvalidNote,
    /// The transaction's anchor/expiry height has passed.
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn network_round_trips() {
        assert_eq!(round_trip(&Network::Main), Network::Main);
        assert_eq!(round_trip(&Network::Test), Network::Test);
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
    fn transfer_proposal_round_trips() {
        let t = TransferProposal {
            id: "abc".to_string(),
            amount_zatoshi: 1_000_000_000,
            anchor_height: 2_880_000,
            next_executable_after_height: 2_880_288,
            expiry_height: 2_880_576,
        };
        assert_eq!(round_trip(&t), t);
    }

    #[test]
    fn migration_schedule_round_trips() {
        let s = MigrationSchedule {
            transfers: vec![TransferProposal {
                id: "t1".to_string(),
                amount_zatoshi: 5,
                anchor_height: 1,
                next_executable_after_height: 2,
                expiry_height: 3,
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
            remaining_orchard_zatoshi: 600_000_000,
            next_transfer_ready_at_height: Some(2_880_864),
        };
        assert_eq!(round_trip(&p), p);
        let none = MigrationProgress {
            next_transfer_ready_at_height: None,
            ..p
        };
        assert_eq!(round_trip(&none), none);
    }

    #[test]
    fn prepared_tx_round_trips() {
        let tx = PreparedTx {
            id: "t1".to_string(),
            txid: "deadbeef".to_string(),
            raw_tx: vec![0x05, 0x00, 0xff],
        };
        assert_eq!(round_trip(&tx), tx);
    }

    #[test]
    fn migration_state_round_trips_each_variant() {
        let progress = MigrationProgress {
            completed_transfers: 1,
            total_transfers: 3,
            remaining_orchard_zatoshi: 1,
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
