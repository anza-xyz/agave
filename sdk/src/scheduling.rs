//! Primitive types relevant to transaction scheduling
#![cfg(feature = "full")]

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SchedulingMode {
    BlockVerification,
    BlockProduction,
}

pub type TaskKey = u128;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MaxAge {
    pub sanitized_epoch: Epoch,
    pub alt_invalidation_slot: Slot,
}

impl MaxAge {
    pub const MAX: Self = Self {
        sanitized_epoch: Epoch::MAX,
        alt_invalidation_slot: Slot::MAX,
    };
}

