//! Error type for the migration engine.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Errors returned by the migration engine. `serde`-derivable so the FFI/JNI glue can marshal
/// them to the platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationError {
    /// The wallet must finish syncing before this operation can proceed.
    NotSynced,
    /// The migration is in a state that does not permit this operation.
    InvalidState(String),
    /// `initialize_post_upgrade` has not been called yet.
    NotInitialized,
    /// A database (SQLite) error.
    Db(String),
    /// An error from the librustzcash backend (proposal, signing, balance read).
    Backend(String),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrationError::NotSynced => write!(f, "wallet must finish syncing first"),
            MigrationError::InvalidState(s) => write!(f, "invalid migration state: {s}"),
            MigrationError::NotInitialized => {
                write!(f, "migration not initialized; call initialize_post_upgrade first")
            }
            MigrationError::Db(s) => write!(f, "database error: {s}"),
            MigrationError::Backend(s) => write!(f, "backend error: {s}"),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<rusqlite::Error> for MigrationError {
    fn from(e: rusqlite::Error) -> Self {
        MigrationError::Db(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_human_readable() {
        assert!(MigrationError::NotSynced.to_string().to_lowercase().contains("sync"));
        assert!(MigrationError::NotInitialized
            .to_string()
            .to_lowercase()
            .contains("initial"));
        assert_eq!(
            MigrationError::InvalidState("bad phase".to_string()).to_string(),
            "invalid migration state: bad phase"
        );
    }

    #[test]
    fn usable_as_std_error() {
        fn takes_error(_: &dyn std::error::Error) {}
        takes_error(&MigrationError::Db("disk full".to_string()));
    }

    #[test]
    fn round_trips_through_serde() {
        let e = MigrationError::Backend("propose failed".to_string());
        let json = serde_json::to_string(&e).unwrap();
        let back: MigrationError = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn rusqlite_error_maps_to_db_variant() {
        let e: MigrationError = rusqlite::Error::QueryReturnedNoRows.into();
        assert!(matches!(e, MigrationError::Db(_)));
    }
}
