#![cfg(feature = "agave-unstable-api")]

//! Cross-domain conformance harnesses for Agave.
//!
//! Each submodule hosts the harness for one domain (gossip today; block,
//! shred, cost, and possibly transaction harnesses may land here later).
//! Everything that depends on protobuf/prost/FFI is gated behind the `ffi`
//! feature.

#[cfg(feature = "ffi")]
pub mod gossip;
