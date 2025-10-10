#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
pub mod accounts_update_notifier;
pub mod block_metadata_notifier;
pub mod block_metadata_notifier_interface;
pub mod entry_notifier;
pub mod geyser_plugin_manager;
pub mod geyser_plugin_service;
pub mod slot_status_notifier;
pub mod slot_status_observer;
pub mod transaction_notifier;

pub use geyser_plugin_manager::GeyserPluginManagerRequest;
