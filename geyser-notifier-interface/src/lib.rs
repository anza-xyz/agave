#![cfg(feature = "agave-unstable-api")]
//! Validator-internal notifier interfaces implemented by the Geyser plugin
//! manager.
//!
//! Validator subsystems (accounts-db, ledger, rpc, gossip, core) invoke these
//! interfaces to surface events; the Geyser plugin manager implements them and
//! fans the events out to loaded plugins. Defining the interfaces here — in a
//! crate that depends only on stable, independently published types — keeps
//! the dependency arrow pointing from the validator internals toward the
//! Geyser crates, rather than the reverse.
//!
//! Nothing in this crate is part of the plugin ABI: plugins only ever see the
//! `GeyserPlugin` trait and `Replica*` types from
//! `agave-geyser-plugin-interface`.

pub mod accounts_update_notifier_interface;
pub mod block_metadata_notifier_interface;
pub mod contact_info_notifier;
pub mod deshred_transaction_notifier_interface;
pub mod slot_notification;
pub mod slot_status_notifier;
