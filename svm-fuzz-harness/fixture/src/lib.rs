//! Context types for fixtures.

pub mod context;
pub mod error;
pub mod invoke;
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/org.solana.sealevel.v1.rs"));
}
