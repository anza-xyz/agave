//! Transaction effects (output).

#[cfg(feature = "conformance")]
use {
    crate::conformance::{account_state::account_to_proto, err::serialized_error_code},
    protosol::protos::{FeeDetails as ProtoFeeDetails, TxnResult as ProtoTxnResult},
    solana_instruction::error::InstructionError,
};
use {
    solana_account::Account,
    solana_fee_structure::FeeDetails,
    solana_pubkey::Pubkey,
    solana_transaction_error::{TransactionError, TransactionResult},
};

/// Represents effects of a single transaction.
pub struct TxnEffects {
    pub executed: bool,
    pub status: TransactionResult<()>,
    pub resulting_accounts: Vec<(Pubkey, Account)>,
    pub return_data: Vec<u8>,
    pub executed_units: u64,
    pub fee_details: FeeDetails,
    pub loaded_accounts_data_size: u64,
    pub logs: Vec<String>,
    pub cu_avail: u64,
}

impl TxnEffects {
    pub(crate) fn from_unprocessed_error(err: TransactionError) -> Self {
        Self {
            executed: false,
            status: Err(err),
            resulting_accounts: vec![],
            return_data: vec![],
            executed_units: 0,
            fee_details: FeeDetails::new(0, 0),
            loaded_accounts_data_size: 0,
            logs: vec![],
            cu_avail: 0,
        }
    }

    /// Returns the resulting account for the given pubkey, if it exists.
    pub fn get_account(&self, pubkey: &Pubkey) -> Option<&Account> {
        self.resulting_accounts
            .iter()
            .find(|(pk, _)| pk == pubkey)
            .map(|(_, account)| account)
    }
}

#[cfg(feature = "conformance")]
impl From<TxnEffects> for ProtoTxnResult {
    fn from(value: TxnEffects) -> Self {
        let (txn_error, instruction_error, custom_error, instruction_error_index) = value
            .status
            .as_ref()
            .err()
            .map(|transaction_error| {
                let (instruction_error, custom_error, instruction_error_index) =
                    match transaction_error {
                        TransactionError::InstructionError(
                            instruction_error_index,
                            instruction_error,
                        ) => {
                            let custom_error = match instruction_error {
                                InstructionError::Custom(custom_error) => *custom_error,
                                _ => 0,
                            };
                            (
                                serialized_error_code(instruction_error),
                                custom_error,
                                (*instruction_error_index).into(),
                            )
                        }
                        _ => (0, 0, 0),
                    };

                (
                    serialized_error_code(transaction_error),
                    instruction_error,
                    custom_error,
                    instruction_error_index,
                )
            })
            .unwrap_or((0, 0, 0, 0));

        let fee_details = Some(ProtoFeeDetails {
            transaction_fee: value.fee_details.transaction_fee(),
            prioritization_fee: value.fee_details.prioritization_fee(),
        });
        let modified_accounts = value
            .resulting_accounts
            .into_iter()
            .map(|(pubkey, account)| account_to_proto((pubkey, account)))
            .collect();

        Self {
            executed: value.executed,
            txn_error,
            instruction_error,
            instruction_error_index,
            custom_error,
            return_data: value.return_data,
            executed_units: value.executed_units,
            fee_details,
            loaded_accounts_data_size: value.loaded_accounts_data_size,
            modified_accounts,
            rollback_accounts: vec![],
        }
    }
}
