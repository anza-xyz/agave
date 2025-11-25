#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![allow(clippy::arithmetic_side_effects)]
mod bucket;
pub mod bucket_api;
mod bucket_item;
pub mod bucket_map;
mod bucket_stats;
mod bucket_storage;
mod index_entry;
mod restart;
pub type MaxSearch = u8;
pub type RefCount = u64;
