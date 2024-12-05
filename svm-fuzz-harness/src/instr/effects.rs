//! Instruction effects (outputs).

use {
    crate::proto::{AcctState as ProtoAcctState, InstrEffects as ProtoInstrEffects},
    solana_account::Account,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
};

pub struct InstrEffects {
    pub result: Option<InstructionError>,
    pub custom_err: Option<u32>,
    pub modified_accounts: Vec<(Pubkey, Account)>,
    pub cu_avail: u64,
    pub return_data: Vec<u8>,
}

impl From<InstrEffects> for ProtoInstrEffects {
    fn from(val: InstrEffects) -> Self {
        ProtoInstrEffects {
            result: val
                .result
                .as_ref()
                .map(instr_err_to_num)
                .unwrap_or_default(),
            custom_err: val.custom_err.unwrap_or_default(),
            modified_accounts: val
                .modified_accounts
                .into_iter()
                .map(|(pubkey, account)| ProtoAcctState {
                    address: pubkey.to_bytes().to_vec(),
                    owner: account.owner.to_bytes().to_vec(),
                    lamports: account.lamports,
                    data: account.data.to_vec(),
                    executable: account.executable,
                    rent_epoch: account.rent_epoch,
                    seed_addr: None,
                })
                .collect(),
            cu_avail: val.cu_avail,
            return_data: val.return_data,
        }
    }
}

pub fn instr_err_to_num(error: &InstructionError) -> i32 {
    let serialized_err = bincode::serialize(error).unwrap();
    i32::from_le_bytes((&serialized_err[0..4]).try_into().unwrap()).saturating_add(1)
}
