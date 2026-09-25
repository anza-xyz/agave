#![cfg(feature = "agave-unstable-api")]
//! The Solana shred wire format.
//!
//! This crate is the layout and nothing else. It knows the section boundaries of all four wire
//! layouts, the header fields, and how to read and write them, it hands out
//! [`ShredView`](view::ShredView) over borrowed bytes and [`ShredViewMut`](view::ShredViewMut) over
//! mutably borrowed bytes.

pub mod constants;
pub mod error;
pub mod headers;
pub mod kind;
pub mod shred_variant;
pub mod view;
