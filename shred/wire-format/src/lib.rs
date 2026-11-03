#![cfg(feature = "agave-unstable-api")]
//! The Solana shred wire format.
//!
//! This crate is the layout and nothing else: the section boundaries of all four wire layouts and
//! the header fields, derived from the wincode schemas of the headers.

pub mod constants;
pub mod error;
pub mod headers;
pub mod kind;
pub mod shred_variant;
