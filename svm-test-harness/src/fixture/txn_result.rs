//! Transaction result (output).

use {solana_account::Account, solana_pubkey::Pubkey, solana_transaction_error::TransactionError};

/// Fee details for a transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeeDetails {
    pub transaction_fee: u64,
    pub prioritization_fee: u64,
}

/// Resulting account state after transaction execution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultingState {
    pub acct_states: Vec<(Pubkey, Account)>,
}

/// Transaction execution result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TxnResult {
    pub executed: bool,
    pub sanitization_error: bool,
    pub resulting_state: Option<ResultingState>,
    pub is_ok: bool,
    pub status: u32,
    pub instruction_error: u32,
    pub instruction_error_index: u32,
    pub custom_error: u32,
    pub return_data: Vec<u8>,
    pub executed_units: u64,
    pub fee_details: Option<FeeDetails>,
    pub loaded_accounts_data_size: u64,
}

pub fn transaction_error_to_err_nums(error: &TransactionError) -> (u32, u32, u32, u32) {
    use solana_instruction_error::InstructionError;

    let (instr_err_no, custom_err_no, instr_err_idx) = match error.clone() {
        TransactionError::InstructionError(idx, instr_err) => {
            let instr_err_no = {
                let serialized = bincode::serialize(&instr_err).unwrap_or(vec![0, 0, 0, 0]);
                u32::from_le_bytes(serialized[0..4].try_into().unwrap()).saturating_add(1)
            };
            let custom_err_no = match instr_err {
                InstructionError::Custom(code) => code,
                _ => 0,
            };
            (instr_err_no, custom_err_no, idx)
        }
        _ => (0, 0, 0),
    };

    let txn_err_no = {
        let serialized = bincode::serialize(error).unwrap_or(vec![0, 0, 0, 0]);
        u32::from_le_bytes(serialized[0..4].try_into().unwrap()).saturating_add(1)
    };

    (
        txn_err_no,
        instr_err_no,
        custom_err_no,
        instr_err_idx.into(),
    )
}

#[cfg(feature = "fuzz")]
use super::proto::{
    FeeDetails as ProtoFeeDetails, ResultingState as ProtoResultingState,
    TxnResult as ProtoTxnResult,
};

#[cfg(feature = "fuzz")]
impl From<TxnResult> for ProtoTxnResult {
    fn from(value: TxnResult) -> Self {
        Self {
            executed: value.executed,
            sanitization_error: value.sanitization_error,
            resulting_state: value.resulting_state.map(|state| ProtoResultingState {
                acct_states: state.acct_states.into_iter().map(Into::into).collect(),
                rent_debits: vec![],
                transaction_rent: 0,
            }),
            rent: 0,
            is_ok: value.is_ok,
            status: value.status,
            instruction_error: value.instruction_error,
            instruction_error_index: value.instruction_error_index,
            custom_error: value.custom_error,
            return_data: value.return_data,
            executed_units: value.executed_units,
            fee_details: value.fee_details.map(|fees| ProtoFeeDetails {
                transaction_fee: fees.transaction_fee,
                prioritization_fee: fees.prioritization_fee,
            }),
            loaded_accounts_data_size: value.loaded_accounts_data_size,
        }
    }
}
