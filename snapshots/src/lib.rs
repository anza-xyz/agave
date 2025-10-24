#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]

mod archive_format;
pub mod hardened_unpack;
pub mod snapshot_config;
pub mod snapshot_hash;
mod snapshot_interval;
mod snapshot_version;
mod unarchive;

pub use {
    archive_format::*, snapshot_interval::SnapshotInterval, snapshot_version::SnapshotVersion,
    unarchive::streaming_unarchive_snapshot,
};
