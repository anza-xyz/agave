//! Instruction context and effects (input + output).

#[cfg(feature = "conformance")]
use {
    super::{
        account_state::{account_from_proto, account_to_proto},
        feature_set::feature_set_from_proto,
    },
    protosol::protos::{InstrContext as ProtoInstrContext, InstrEffects as ProtoInstrEffects},
    solana_instruction::AccountMeta,
};
use {
    solana_account::Account,
    solana_instruction::{Instruction, error::InstructionError},
    solana_program_runtime::execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey,
    solana_svm_feature_set::SVMFeatureSet,
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
impl From<InstrEffects> for ProtoInstrEffects {
    fn from(value: InstrEffects) -> Self {
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
                .map(|error| {
                    let serialized_err = bincode::serialize(error).unwrap();
                    i32::from_le_bytes((&serialized_err[0..4]).try_into().unwrap())
                        .saturating_add(1)
                })
                .unwrap_or_default(),
            custom_err: custom_err.unwrap_or_default(),
            modified_accounts: resulting_accounts
                .into_iter()
                .map(account_to_proto)
                .collect(),
            cu_avail,
            return_data,
        }
    }
}
