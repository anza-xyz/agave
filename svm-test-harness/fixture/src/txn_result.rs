//! Transaction result (output).

use {
    solana_account::Account, solana_fee_structure::FeeDetails, solana_pubkey::Pubkey,
    solana_transaction_error::TransactionResult,
};

/// Transaction execution result.
#[derive(Clone, Debug, PartialEq)]
pub struct TxnResult {
    pub status: TransactionResult<()>,
    pub resulting_accounts: Vec<(Pubkey, Account)>,
    pub return_data: Vec<u8>,
    pub executed_units: u64,
    pub fee_details: FeeDetails,
    pub loaded_accounts_data_size: u64,
}

#[cfg(feature = "fuzz")]
use {
    super::proto::{
        FeeDetails as ProtoFeeDetails, ResultingState as ProtoResultingState,
        TxnResult as ProtoTxnResult,
    },
    agave_precompiles::get_precompile,
    solana_instruction_error::InstructionError,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_error::TransactionError,
};

#[cfg(feature = "fuzz")]
fn transaction_error_to_err_nums(error: &TransactionError) -> (u32, u32, u32, u32) {
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

/// Convert a `TxnResult` to a protobuf `TxnResult`, filtering accounts to only
/// those marked writable in the message.
#[cfg(feature = "fuzz")]
pub fn txn_result_to_proto(result: TxnResult, message: &impl SVMMessage) -> ProtoTxnResult {
    let acct_states: Vec<_> = result
        .resulting_accounts
        .into_iter()
        .enumerate()
        .filter(|&(i, _)| message.is_writable(i))
        .map(|(_, acct)| acct.into())
        .collect();

    let (
        executed,
        sanitization_error,
        is_ok,
        status,
        instruction_error,
        instruction_error_index,
        custom_error,
    ) = match &result.status {
        Ok(()) => (true, false, true, 0, 0, 0, 0),
        Err(err) => {
            let (status, instr_err, custom_err, instr_err_idx) = transaction_error_to_err_nums(err);
            // Set custom err to 0 if the failing instruction is a precompile.
            let custom_err = message
                .instructions_iter()
                .nth(instr_err_idx as usize)
                .and_then(|instr| {
                    message
                        .account_keys()
                        .get(usize::from(instr.program_id_index))
                        .map(|program_id| {
                            if get_precompile(program_id, |_| true).is_some() {
                                0
                            } else {
                                custom_err
                            }
                        })
                })
                .unwrap_or(custom_err);
            (
                true,
                false,
                false,
                status,
                instr_err,
                instr_err_idx,
                custom_err,
            )
        }
    };

    ProtoTxnResult {
        executed,
        sanitization_error,
        resulting_state: Some(ProtoResultingState {
            acct_states,
            rent_debits: vec![],
            transaction_rent: 0,
        }),
        rent: 0,
        is_ok,
        status,
        instruction_error,
        instruction_error_index,
        custom_error,
        return_data: result.return_data,
        executed_units: result.executed_units,
        fee_details: Some(ProtoFeeDetails {
            transaction_fee: result.fee_details.transaction_fee(),
            prioritization_fee: result.fee_details.prioritization_fee(),
        }),
        loaded_accounts_data_size: result.loaded_accounts_data_size,
    }
}
