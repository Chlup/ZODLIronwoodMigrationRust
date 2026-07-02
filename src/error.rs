//! Error type for the migration engine.

use std::fmt;

#[cfg(feature = "librustzcash-backend")]
use zcash_client_sqlite::error::SqliteClientError;

/// Why an operation was rejected because the migration was in the wrong state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidStateError {
    /// There is no active migration run for the account.
    NoActiveRun,
    /// A persisted run carried a phase string the engine does not recognise.
    UnknownPhase(String),
    /// The run is in a phase that does not permit this operation.
    WrongPhase {
        expected: &'static str,
        found: String,
    },
    /// The migration has already completed.
    AlreadyComplete,
    /// The operation does not apply in the current state (short reason).
    NotApplicable(&'static str),
}

impl fmt::Display for InvalidStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidStateError::NoActiveRun => write!(f, "no active migration run"),
            InvalidStateError::UnknownPhase(p) => write!(f, "unknown migration phase: {p}"),
            InvalidStateError::WrongPhase { expected, found } => {
                write!(
                    f,
                    "wrong migration phase: expected {expected}, found {found}"
                )
            }
            InvalidStateError::AlreadyComplete => write!(f, "migration already complete"),
            InvalidStateError::NotApplicable(why) => write!(f, "operation not applicable: {why}"),
        }
    }
}

/// Errors returned by the migration engine.
///
/// Wraps the underlying error types (`rusqlite::Error`, `SqliteClientError`) rather than
/// stringly-typed messages. Intentionally **not** `serde`-derivable; the FFI/JNI glue marshals it
/// via [`MigrationError::error_code`] plus the `Display` string.
#[derive(Debug)]
pub enum MigrationError {
    /// The wallet must finish syncing before this operation can proceed.
    NotSynced,
    /// `initialize_post_upgrade` has not been called yet.
    NotInitialized,
    /// The migration is in a state that does not permit this operation.
    InvalidState(InvalidStateError),
    /// A database (SQLite) error from the engine's own tables.
    Db(rusqlite::Error),
    /// An error from the `zcash_client_sqlite` wallet backend (balance/anchor/data access).
    #[cfg(feature = "librustzcash-backend")]
    Backend(SqliteClientError),
    /// An error from the PCZT construction / proving / signing / extraction pipeline, whose sources
    /// are heterogeneous and share no single common type.
    #[cfg(feature = "librustzcash-backend")]
    Pipeline(String),
    /// A capability the engine relies on is not yet available in the pinned librustzcash. Currently
    /// used for the Ironwood balance read: upstream (eb828ca) leaves Ironwood pool/note tracking as
    /// `todo!()`, so the wallet cannot report an Ironwood balance yet.
    Unsupported(&'static str),
}

impl MigrationError {
    /// A stable numeric code for the FFI/JNI boundary.
    pub fn error_code(&self) -> u32 {
        match self {
            MigrationError::NotSynced => 1,
            MigrationError::NotInitialized => 2,
            MigrationError::InvalidState(_) => 3,
            MigrationError::Db(_) => 4,
            #[cfg(feature = "librustzcash-backend")]
            MigrationError::Backend(_) => 5,
            #[cfg(feature = "librustzcash-backend")]
            MigrationError::Pipeline(_) => 6,
            MigrationError::Unsupported(_) => 7,
        }
    }
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrationError::NotSynced => write!(f, "wallet must finish syncing first"),
            MigrationError::NotInitialized => {
                write!(
                    f,
                    "migration not initialized; call initialize_post_upgrade first"
                )
            }
            MigrationError::InvalidState(e) => write!(f, "invalid migration state: {e}"),
            MigrationError::Db(e) => write!(f, "database error: {e}"),
            #[cfg(feature = "librustzcash-backend")]
            MigrationError::Backend(e) => write!(f, "wallet backend error: {e}"),
            #[cfg(feature = "librustzcash-backend")]
            MigrationError::Pipeline(e) => write!(f, "pczt pipeline error: {e}"),
            MigrationError::Unsupported(why) => {
                write!(f, "unsupported by pinned librustzcash: {why}")
            }
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MigrationError::Db(e) => Some(e),
            #[cfg(feature = "librustzcash-backend")]
            MigrationError::Backend(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(e: rusqlite::Error) -> Self {
        MigrationError::Db(e)
    }
}

#[cfg(feature = "librustzcash-backend")]
impl From<SqliteClientError> for MigrationError {
    fn from(e: SqliteClientError) -> Self {
        MigrationError::Backend(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable_and_display_readable() {
        assert_eq!(MigrationError::NotSynced.error_code(), 1);
        assert_eq!(MigrationError::NotInitialized.error_code(), 2);
        let e = MigrationError::InvalidState(InvalidStateError::NoActiveRun);
        assert_eq!(e.error_code(), 3);
        assert!(e.to_string().to_lowercase().contains("no active"));
        let db: MigrationError = rusqlite::Error::QueryReturnedNoRows.into();
        assert_eq!(db.error_code(), 4);
    }

    #[test]
    fn usable_as_std_error_with_source() {
        use std::error::Error;
        let e = MigrationError::Db(rusqlite::Error::QueryReturnedNoRows);
        fn takes_error(_: &dyn std::error::Error) {}
        takes_error(&e);
        assert!(e.source().is_some());
    }

    #[test]
    fn not_synced_and_not_initialized_display() {
        assert!(MigrationError::NotSynced
            .to_string()
            .to_lowercase()
            .contains("sync"));
        assert!(MigrationError::NotInitialized
            .to_string()
            .to_lowercase()
            .contains("initial"));
    }

    #[test]
    fn invalid_state_display_variants() {
        assert!(InvalidStateError::AlreadyComplete
            .to_string()
            .contains("complete"));
        assert_eq!(
            InvalidStateError::WrongPhase {
                expected: "ready",
                found: "broadcasting".to_string()
            }
            .to_string(),
            "wrong migration phase: expected ready, found broadcasting"
        );
    }
}
