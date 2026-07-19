//! Invoke-context callbacks shared by the conformance harnesses.

use solana_svm_callback::InvokeContextCallback;

/// Default callback. No precompile support.
pub struct DefaultCallback;

impl InvokeContextCallback for DefaultCallback {}

/// Conformance callback with transaction-scoped epoch stake.
#[cfg(feature = "conformance")]
#[derive(Default)]
pub struct ConformanceCallback {
    pub epoch_total_stake: u64,
}

#[cfg(feature = "conformance")]
impl InvokeContextCallback for ConformanceCallback {
    fn get_epoch_stake(&self) -> u64 {
        self.epoch_total_stake
    }
}
