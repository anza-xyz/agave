//! Module to provide easy construction of C-compatible interfaces for
//! interacting with transactions from valid transaction references.
//!

use {
    agave_transaction_ffi::{
        AccountCallback, Instruction, InstructionCallback, TransactionInterface, TransactionPtr,
    },
    solana_svm_transaction::svm_transaction::SVMTransaction,
};

/// Given a reference to any type that implements `SVMTransaction`, create a
/// `TransactionInterface` that can be used to interact with the transaction in
/// C-compatible code. This interface is only valid for the lifetime of the
/// transaction reference, which cannot be guaranteed by this function interface.
#[allow(dead_code)]
pub unsafe fn create_transaction_interface<Tx: SVMTransaction>(
    transaction: &Tx,
) -> TransactionInterface {
    extern "C" fn num_signatures<Tx: SVMTransaction>(transaction_ptr: TransactionPtr) -> usize {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.signatures().len()
    }

    extern "C" fn signatures<Tx: SVMTransaction>(transaction_ptr: TransactionPtr) -> *const u8 {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.signatures().as_ptr() as *const u8
    }

    extern "C" fn num_write_locks<Tx: SVMTransaction>(transaction_ptr: TransactionPtr) -> u64 {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.num_write_locks()
    }

    extern "C" fn recent_blockhash<Tx: SVMTransaction>(
        transaction_ptr: TransactionPtr,
    ) -> *const u8 {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.recent_blockhash().as_ref().as_ptr()
    }

    extern "C" fn num_instructions<Tx: SVMTransaction>(transaction_ptr: TransactionPtr) -> usize {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.num_instructions()
    }

    extern "C" fn iter_instructions<Tx: SVMTransaction>(
        transaction_ptr: TransactionPtr,
        callback: InstructionCallback,
    ) {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        for instruction in transaction.instructions_iter() {
            let instruction = Instruction {
                program_index: instruction.program_id_index,
                num_accounts: instruction.accounts.len() as u16,
                accounts: instruction.accounts.as_ptr(),
                data_len: instruction.data.len() as u16,
                data: instruction.data.as_ptr(),
            };

            if !unsafe { (callback.callback)(callback.state, instruction) } {
                break;
            }
        }
    }

    extern "C" fn num_accounts<Tx: SVMTransaction>(transaction_ptr: TransactionPtr) -> usize {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.account_keys().len()
    }

    extern "C" fn get_account<Tx: SVMTransaction>(
        transaction_ptr: TransactionPtr,
        index: usize,
    ) -> *const u8 {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction
            .account_keys()
            .get(index)
            .map_or(core::ptr::null(), |key| key.as_ref().as_ptr())
    }

    extern "C" fn is_writable<Tx: SVMTransaction>(
        transaction_ptr: TransactionPtr,
        index: usize,
    ) -> bool {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.is_writable(index)
    }

    extern "C" fn is_signer<Tx: SVMTransaction>(
        transaction_ptr: TransactionPtr,
        index: usize,
    ) -> bool {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.is_signer(index)
    }

    extern "C" fn is_invoked<Tx: SVMTransaction>(
        transaction_ptr: TransactionPtr,
        index: usize,
    ) -> bool {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        transaction.is_invoked(index)
    }

    extern "C" fn iter_accounts<Tx: SVMTransaction>(
        transaction_ptr: TransactionPtr,
        callback: AccountCallback,
    ) {
        let transaction = unsafe { &*(transaction_ptr as *const Tx) };
        for account_key in transaction.account_keys().iter() {
            let account_key = account_key.as_ref().as_ptr();
            if !unsafe { (callback.callback)(callback.state, account_key) } {
                break;
            }
        }
    }

    TransactionInterface {
        transaction_ptr: transaction as *const Tx as *const core::ffi::c_void,
        num_signatures_fn: num_signatures::<Tx>,
        signatures_fn: signatures::<Tx>,
        num_write_locks_fn: num_write_locks::<Tx>,
        recent_blockhash_fn: recent_blockhash::<Tx>,
        num_instructions_fn: num_instructions::<Tx>,
        iter_instructions_fn: iter_instructions::<Tx>,
        num_accounts_fn: num_accounts::<Tx>,
        get_account_fn: get_account::<Tx>,
        is_writable_fn: is_writable::<Tx>,
        is_signer_fn: is_signer::<Tx>,
        is_invoked_fn: is_invoked::<Tx>,
        iter_accounts_fn: iter_accounts::<Tx>,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{
            hash::Hash, pubkey::Pubkey, signature::Keypair, system_transaction,
            transaction::SanitizedTransaction,
        },
    };

    #[test]
    fn test_create_transaction_interface_with_sanitized_transaction() {
        let simple_transfer =
            SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &Keypair::new(),
                &Pubkey::new_unique(),
                1,
                Hash::default(),
            ));

        // SAFETY: The interface is valid for as long as the transaction reference is valid.
        let interface = unsafe { create_transaction_interface(&simple_transfer) };

        // Verify signature len and address are the same.
        let signatures = simple_transfer.signatures();
        assert_eq!(signatures.len(), unsafe {
            (interface.num_signatures_fn)(interface.transaction_ptr)
        });
        assert_eq!(signatures.as_ptr(), unsafe {
            (interface.signatures_fn)(interface.transaction_ptr) as *const _
        },);

        // Verify requested write locks are the same.
        assert_eq!(simple_transfer.message().num_write_locks(), unsafe {
            (interface.num_write_locks_fn)(interface.transaction_ptr)
        });

        // Verify recent blockhash is the same.
        assert_eq!(
            simple_transfer
                .message()
                .recent_blockhash()
                .as_ref()
                .as_ptr(),
            unsafe { (interface.recent_blockhash_fn)(interface.transaction_ptr) }
        );

        // Verify number of instructions is the same.
        assert_eq!(simple_transfer.message().instructions().len(), unsafe {
            (interface.num_instructions_fn)(interface.transaction_ptr)
        });

        struct CallbackState<'a> {
            count: usize,
            transaction: &'a SanitizedTransaction,
        }
        let mut state = CallbackState {
            count: 0,
            transaction: &simple_transfer,
        };

        extern "C" fn instruction_callback(
            state: *mut core::ffi::c_void,
            instruction: Instruction,
        ) -> bool {
            let state = unsafe { &mut *(state as *mut CallbackState) };

            // Verify the instruction data is the same.
            let actual_instruction = state
                .transaction
                .message()
                .instructions()
                .get(state.count)
                .unwrap();
            assert_eq!(
                actual_instruction.program_id_index,
                instruction.program_index
            );
            assert_eq!(
                actual_instruction.accounts.len(),
                instruction.num_accounts as usize
            );
            assert_eq!(actual_instruction.accounts.as_ptr(), instruction.accounts);
            assert_eq!(actual_instruction.data.len(), instruction.data_len as usize);
            assert_eq!(actual_instruction.data.as_ptr(), instruction.data);

            // Update count
            state.count += 1;
            true
        }

        unsafe {
            (interface.iter_instructions_fn)(
                interface.transaction_ptr,
                InstructionCallback {
                    state: &mut state as *mut _ as *mut core::ffi::c_void,
                    callback: instruction_callback,
                },
            );
        }
        assert_eq!(state.count, simple_transfer.message().instructions().len());

        // Verify number of accounts is the same.
        assert_eq!(simple_transfer.message().account_keys().len(), unsafe {
            (interface.num_accounts_fn)(interface.transaction_ptr)
        });

        // Verify account and properties are the same.
        for (i, account_key) in simple_transfer.message().account_keys().iter().enumerate() {
            assert_eq!(account_key.as_ref().as_ptr(), unsafe {
                (interface.get_account_fn)(interface.transaction_ptr, i)
            });

            assert_eq!(simple_transfer.message().is_writable(i), unsafe {
                (interface.is_writable_fn)(interface.transaction_ptr, i)
            });

            assert_eq!(simple_transfer.message().is_signer(i), unsafe {
                (interface.is_signer_fn)(interface.transaction_ptr, i)
            });

            assert_eq!(simple_transfer.message().is_invoked(i), unsafe {
                (interface.is_invoked_fn)(interface.transaction_ptr, i)
            });
        }
        // Verify return out of bounds is null or false
        let out_of_bounds_index = simple_transfer.message().account_keys().len();
        assert_eq!(core::ptr::null(), unsafe {
            (interface.get_account_fn)(interface.transaction_ptr, out_of_bounds_index)
        });
        assert!(!unsafe {
            (interface.is_writable_fn)(interface.transaction_ptr, out_of_bounds_index)
        });
        assert!(!unsafe {
            (interface.is_signer_fn)(interface.transaction_ptr, out_of_bounds_index)
        });
        assert!(!unsafe {
            (interface.is_invoked_fn)(interface.transaction_ptr, out_of_bounds_index)
        });

        struct AccountCallbackState<'a> {
            count: usize,
            transaction: &'a SanitizedTransaction,
        }

        extern "C" fn account_callback(
            state: *mut core::ffi::c_void,
            account_key: *const u8,
        ) -> bool {
            let state = unsafe { &mut *(state as *mut AccountCallbackState) };

            // Verify the account key is the same.
            let actual_account_key = state
                .transaction
                .message()
                .account_keys()
                .get(state.count)
                .unwrap();
            assert_eq!(actual_account_key.as_ref().as_ptr(), account_key);

            // Update count
            state.count += 1;
            true
        }

        let mut state = AccountCallbackState {
            count: 0,
            transaction: &simple_transfer,
        };

        unsafe {
            (interface.iter_accounts_fn)(
                interface.transaction_ptr,
                AccountCallback {
                    state: &mut state as *mut _ as *mut core::ffi::c_void,
                    callback: account_callback,
                },
            );
        }
        assert_eq!(state.count, simple_transfer.message().account_keys().len());
    }
}
