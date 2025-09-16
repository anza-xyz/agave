//! Instruction effects (output).
use {
    crate::proto::InstrEffects as ProtoInstrEffects, solana_account::Account,
    solana_instruction_error::InstructionError, solana_pubkey::Pubkey,
};

/// Represents the effects of a single instruction.
pub struct InstrEffects {
    pub result: Option<InstructionError>,
    pub custom_err: Option<u32>,
    pub modified_accounts: Vec<(Pubkey, Account)>,
    pub cu_avail: u64,
    pub return_data: Vec<u8>,
}

impl From<InstrEffects> for ProtoInstrEffects {
    fn from(value: InstrEffects) -> Self {
        let InstrEffects {
            result,
            custom_err,
            modified_accounts,
            cu_avail,
            return_data,
            ..
        } = value;

        Self {
            result: result.as_ref().map(instr_err_to_num).unwrap_or_default(),
            custom_err: custom_err.unwrap_or_default(),
            modified_accounts: modified_accounts.into_iter().map(Into::into).collect(),
            cu_avail,
            return_data,
        }
    }
}

fn instr_err_to_num(error: &InstructionError) -> i32 {
    let serialized_err = bincode::serialize(error).unwrap();
    i32::from_le_bytes((&serialized_err[0..4]).try_into().unwrap()).saturating_add(1)
}
