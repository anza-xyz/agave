//! Converts between Firedancer's protobuf payloads and Solana SDK types for
//! use in Agave's SVM.

pub mod account_state;
pub mod error;
pub mod feature_set;
pub mod instr_context;
pub mod instr_effects;
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/org.solana.sealevel.v1.rs"));
}
