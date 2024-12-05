//! Solana SVM fuzzing harness.

pub mod error;
pub mod instr;
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/org.solana.sealevel.v1.rs"));
}
