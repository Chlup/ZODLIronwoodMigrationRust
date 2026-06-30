// Copyright 2026 ZODL contributors.
// Portions ported from vizor-wallet (Chainapsis), licensed under Apache-2.0.
//
// Licensed under the Apache License, Version 2.0. See the LICENSE file.

//! # zodl_ironwood_migration
//!
//! Shared, pure-Rust engine for migrating a wallet's funds from the Orchard pool to the
//! Ironwood pool, ported from the vizor-wallet reference implementation.
//!
//! The crate is consumed by the iOS SDK (`libzcashlc`) and the Android SDK (`backend-lib`)
//! through thin FFI/JNI glue. It is **synchronous, runtime-free, and does no networking**:
//! it prepares, signs, and persists migration transactions and decides which one is due
//! next, but the actual network broadcast is performed by the platform layer (where Tor and
//! the lightwalletd connection already live). See `docs/superpowers/specs/` for the design.
//!
//! ## Layering
//!
//! * **core** (always compiled, valargroup-free): [`types`], [`error`], `denominations`,
//!   `scheduling`, `state`, `store`, `network`. Fully unit-tested.
//! * **backend** (behind the `librustzcash-backend` feature): the `ReservedInputSource`
//!   adapter, the propose/sign/balance calls against `zcash_client_sqlite::WalletDb`, and
//!   the [`MigrationContext`] facade. Compiled against the valargroup librustzcash fork.

// ----- core modules (no valargroup dependency) -----
pub mod error;
pub mod types;

mod denominations;
mod network;
mod scheduling;
mod state;
mod store;

// ----- backend modules (added during backend bring-up, gated on the feature) -----
// #[cfg(feature = "librustzcash-backend")] mod backend;
// #[cfg(feature = "librustzcash-backend")] mod context;
// #[cfg(feature = "librustzcash-backend")] mod reserved_source;
// #[cfg(feature = "librustzcash-backend")] pub use context::MigrationContext;
