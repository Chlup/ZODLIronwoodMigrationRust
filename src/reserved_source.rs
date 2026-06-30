//! `ReservedInputSource`: an `InputSource` adapter over `zcash_client_sqlite::WalletDb` that
//! excludes notes reserved in the current batch and migration-locked notes (ported from vizor's
//! `send.rs`). Implemented in its own task.
