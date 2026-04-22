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

/// Numeric error codes derived from a [`TransactionError`], matching the
/// proto fixture format.
#[cfg(feature = "fuzz")]
struct TransactionErrorNums {
    /// Discriminant of the top-level `TransactionError`, offset by +1 so that
    /// `0` stays reserved for success.
    txn_err: u32,
    /// Discriminant of the inner `InstructionError` (if any), offset by +1 so
    /// that `0` stays reserved for "no instruction error".
    instr_err: u32,
    /// Payload of `InstructionError::Custom`, zero otherwise.
    custom_err: u32,
    /// Index of the instruction that failed, zero if the error is not an
    /// `InstructionError`.
    instr_err_idx: u32,
}

#[cfg(feature = "fuzz")]
fn transaction_error_to_err_nums(error: &TransactionError) -> TransactionErrorNums {
    let (instr_err, custom_err, instr_err_idx) = match error.clone() {
        TransactionError::InstructionError(idx, instr_err) => {
            let instr_err_no = bincode::serialize(&instr_err)
                .ok()
                .and_then(|b| {
                    b.get(0..4)
                        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                })
                .map(|n| n.saturating_add(1))
                .unwrap_or(0);
            let custom_err_no = match instr_err {
                InstructionError::Custom(code) => code,
                _ => 0,
            };
            (instr_err_no, custom_err_no, idx)
        }
        _ => (0, 0, 0),
    };

    let txn_err = bincode::serialize(error)
        .ok()
        .and_then(|b| {
            b.get(0..4)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        })
        .map(|n| n.saturating_add(1))
        .unwrap_or(0);

    TransactionErrorNums {
        txn_err,
        instr_err,
        custom_err,
        instr_err_idx: instr_err_idx.into(),
    }
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

    let (executed, is_ok, status, instruction_error, instruction_error_index, custom_error) =
        match &result.status {
            Ok(()) => (true, true, 0, 0, 0, 0),
            Err(err) => {
                let nums = transaction_error_to_err_nums(err);
                // Set custom err to 0 if the failing instruction is a precompile.
                let custom_err = message
                    .instructions_iter()
                    .nth(nums.instr_err_idx as usize)
                    .and_then(|instr| {
                        message
                            .account_keys()
                            .get(usize::from(instr.program_id_index))
                            .map(|program_id| {
                                if get_precompile(program_id, |_| true).is_some() {
                                    0
                                } else {
                                    nums.custom_err
                                }
                            })
                    })
                    .unwrap_or(nums.custom_err);
                (
                    true,
                    false,
                    nums.txn_err,
                    nums.instr_err,
                    nums.instr_err_idx,
                    custom_err,
                )
            }
        };

    ProtoTxnResult {
        executed,
        // TODO: Duplicate of `executed`; remove from proto definition on next
        // protosol crate bump.
        sanitization_error: false,
        resulting_state: Some(ProtoResultingState {
            acct_states,
            // TODO: Rent fields are stubbed; they are being removed from the
            // proto definition and will go away once the protosol crate is
            // bumped.
            rent_debits: vec![],
            transaction_rent: 0,
        }),
        // TODO: Rent fields are stubbed; they are being removed from the
        // proto definition and will go away once the protosol crate is
        // bumped.
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
