//! Instruction effects (output).

#[cfg(feature = "conformance")]
use {
    super::context::InstrContext,
    crate::conformance::{
        account_state::account_to_proto, direct_mapping::direct_mapping_handle_cu_exhaustion,
        err::serialized_error_code, fd_hash::fd_hash_or_zero,
    },
    agave_precompiles::is_precompile,
    protosol::protos::InstrEffects as ProtoInstrEffects,
};
use {solana_account::Account, solana_instruction_error::InstructionError, solana_pubkey::Pubkey};

/// Represents the effects of a single instruction.
pub struct InstrEffects {
    pub result: Option<InstructionError>,
    pub custom_err: Option<u32>,
    pub resulting_accounts: Vec<(Pubkey, Account)>,
    pub cu_avail: u64,
    pub return_data: Vec<u8>,
    pub logs: Vec<String>,
}

impl InstrEffects {
    /// Returns the resulting account for the given pubkey, if it exists.
    pub fn get_account(&self, pubkey: &Pubkey) -> Option<&Account> {
        self.resulting_accounts
            .iter()
            .find(|(pk, _)| pk == pubkey)
            .map(|(_, acc)| acc)
    }
}

#[cfg(feature = "conformance")]
impl From<&InstrEffects> for ProtoInstrEffects {
    fn from(value: &InstrEffects) -> Self {
        let InstrEffects {
            result,
            custom_err,
            resulting_accounts,
            cu_avail,
            return_data,
            ..
        } = value;

        Self {
            result: result
                .as_ref()
                .map(|error| serialized_error_code(error) as i32)
                .unwrap_or_default(),
            custom_err: custom_err.unwrap_or_default(),
            modified_accounts: resulting_accounts
                .iter()
                .map(|(pubkey, account)| account_to_proto(pubkey, account))
                .collect(),
            cu_avail: *cu_avail,
            return_data_hash: fd_hash_or_zero(return_data),
        }
    }
}

/// Convert harness effects into the effects a fixture expects, applying the
/// transformations needed to compare against Firedancer.
///
/// These are specific to Firedancer's conformance tooling, so they stay out of
/// the harness, whose callers should see what actually happened.
#[cfg(feature = "conformance")]
pub fn apply_firedancer_transformations(
    input: &InstrContext,
    effects: &InstrEffects,
) -> ProtoInstrEffects {
    let mut transformed = ProtoInstrEffects::from(effects);

    // Precompile verification failures surface as `Custom`, but Firedancer
    // reports a custom error code of 0 for precompiles.
    if effects.custom_err.is_some() && is_precompile(&input.instruction.program_id, |_| true) {
        transformed.custom_err = 0;
    }

    // TODO: Firedancer's tooling compares resulting account contents even
    // when execution fails, so the harness must report them. Account
    // contents are not meaningful on error (partial writes can diverge based
    // on timing, e.g. with direct mapping or builtins), so once the tooling
    // supports it, the harness should skip the account comparison on error
    // entirely, which would also make the CU-exhaustion workaround below
    // unnecessary.
    direct_mapping_handle_cu_exhaustion(
        input.feature_set.virtual_address_space_adjustments,
        effects.cu_avail,
        effects.result.is_some(),
        transformed.modified_accounts.iter_mut(),
    );

    transformed
}
