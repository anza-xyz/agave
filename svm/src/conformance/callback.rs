//! Invoke-context callbacks shared by the conformance harnesses.

use solana_svm_callback::InvokeContextCallback;
#[cfg(feature = "conformance")]
use {
    agave_feature_set::FeatureSet,
    agave_precompiles::{get_precompile, is_precompile},
    solana_precompile_error::PrecompileError,
    solana_pubkey::Pubkey,
};

/// Default callback. No precompile support.
pub struct DefaultCallback;

impl InvokeContextCallback for DefaultCallback {}

/// Conformance callback. Full precompile support across all features.
#[cfg(feature = "conformance")]
pub struct ConformanceCallback;

#[cfg(feature = "conformance")]
impl InvokeContextCallback for ConformanceCallback {
    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        is_precompile(program_id, |_| true)
    }

    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> Result<(), PrecompileError> {
        if let Some(precompile) = get_precompile(program_id, |_| true) {
            precompile.verify(data, &instruction_datas, &FeatureSet::all_enabled())
        } else {
            Err(PrecompileError::InvalidPublicKey)
        }
    }
}
