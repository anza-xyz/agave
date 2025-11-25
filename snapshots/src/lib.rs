#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

mod archive;
mod archive_format;
pub mod error;
pub mod hardened_unpack;
mod kind;
pub mod paths;
pub mod snapshot_archive_info;
pub mod snapshot_config;
pub mod snapshot_hash;
mod snapshot_interval;
mod snapshot_version;
mod unarchive;

pub type Result<T> = std::result::Result<T, error::SnapshotError>;

pub use {
    archive::archive_snapshot,
    archive_format::*,
    kind::{SnapshotArchiveKind, SnapshotKind},
    snapshot_interval::SnapshotInterval,
    snapshot_version::SnapshotVersion,
    unarchive::{streaming_unarchive_snapshot, unpack_genesis_archive},
};
