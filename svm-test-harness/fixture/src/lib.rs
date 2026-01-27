//! Fixture types for SVM testing.
//!
//! This module provides native Rust types for testing program execution.
//! When the `fuzz` feature is enabled, it also includes conversions
//! between Firedancer's protobuf payloads and Solana SDK types.

pub mod account_state;
pub mod error;
pub mod feature_set;
pub mod instr_context;
pub mod instr_effects;

#[cfg(feature = "fuzz")]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/org.solana.sealevel.v1.rs"));
}
