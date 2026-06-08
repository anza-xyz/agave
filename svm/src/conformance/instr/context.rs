//! Instruction context (input).

#[cfg(feature = "conformance")]
use {
    crate::conformance::{account_state::account_from_proto, feature_set::feature_set_from_proto},
    protosol::protos::InstrContext as ProtoInstrContext,
    solana_instruction::AccountMeta,
};
use {
    solana_account::Account, solana_instruction::Instruction,
    solana_program_runtime::execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey, solana_svm_feature_set::SVMFeatureSet,
};

/// Inputs to a single instruction.
pub struct InstrContext {
    pub feature_set: SVMFeatureSet,
    pub accounts: Vec<(Pubkey, Account)>,
    pub instruction: Instruction,
    pub cu_avail: u64,
}

impl InstrContext {
    /// Create a new [`InstrContext`] with the default compute unit budget
    /// (200,000 CUs).
    pub fn new_with_default_budget(
        feature_set: SVMFeatureSet,
        accounts: Vec<(Pubkey, Account)>,
        instruction: Instruction,
    ) -> Self {
        Self {
            feature_set,
            accounts,
            instruction,
            cu_avail: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        }
    }
}

#[cfg(feature = "conformance")]
impl From<ProtoInstrContext> for InstrContext {
    fn from(value: ProtoInstrContext) -> Self {
        let program_id = Pubkey::try_from(value.program_id).expect("invalid program_id bytes");

        let feature_set = value
            .features
            .as_ref()
            .map(feature_set_from_proto)
            .unwrap_or_default()
            .runtime_features();

        let accounts: Vec<(Pubkey, Account)> =
            value.accounts.into_iter().map(account_from_proto).collect();

        let instruction_accounts = value
            .instr_accounts
            .into_iter()
            .map(|acct| {
                let pubkey = accounts
                    .get(acct.index as usize)
                    .map(|(pk, _)| *pk)
                    .expect("instruction account index out of bounds");
                AccountMeta {
                    pubkey,
                    is_signer: acct.is_signer,
                    is_writable: acct.is_writable,
                }
            })
            .collect::<Vec<_>>();

        assert!(
            instruction_accounts.len() <= 128,
            "too many instruction accounts",
        );

        let instruction = Instruction {
            accounts: instruction_accounts,
            data: value.data,
            program_id,
        };

        let cu_avail = value.cu_avail;

        Self {
            feature_set,
            accounts,
            instruction,
            cu_avail,
        }
    }
}
