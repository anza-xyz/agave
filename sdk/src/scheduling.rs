//! Primitive types relevant to transaction scheduling
#![cfg(feature = "full")]

#[derive(Debug, Clone, Copy)]
pub enum SchedulingMode {
    BlockVerification,
    BlockProduction,
}

pub type TaskKey = u128;
