use {
    solana_account::ReadableAccount, solana_pubkey::Pubkey, solana_sbpf::ebpf::MM_REGION_SIZE,
    solana_svm_feature_set::SVMFeatureSet, solana_transaction_context::TransactionContext,
    std::slice,
};

/// This is how a slice is represented in the VM.
/// It should be merged with VmSlice in a future refactor.
#[repr(C)]
struct GuestSliceReference {
    pointer: u64,
    length: u64,
}

/// The Return data scratchpad
#[repr(C)]
struct ReturnDataScratchpad {
    /// The key of the last program to write in the scratchpad
    pubkey: Pubkey,
    /// Reference to the slice
    slice: GuestSliceReference,
}

/// `GuestTransactionAccount` is how a transaction appears to programs in the virtual machine
#[repr(C)]
struct GuestTransactionAccount {
    pubkey: Pubkey,
    owner: Pubkey,
    lamports: u64,
    data: GuestSliceReference,
}

#[repr(C)]
struct GuestTransactionContext {
    return_data_scratchpad: ReturnDataScratchpad,
    cpi_scratchpad: GuestSliceReference,
    /// The index of the current executing instruction
    instruction_idx: u64,
    /// The number of instructions in the transaction
    instruction_num: u64,
    /// The number of accounts in the transaction
    accounts_no: u64,
}

/// `RuntimeGuestTransaction` contains both the `GuestTransactionContext` and an array of
/// `GuestTransactionAccount`. It is the memory region in ABIv2 that contains the transaction
/// information.
pub struct RuntimeGuestTransaction {
    data: Box<[u8]>,
}

// These values should be declared in the sbpf crate
const RETURN_DATA_SCRATCHPAD_ADDRESS: u64 = MM_REGION_SIZE * 5;
const ACCOUNT_DATA_VM_ADDRESS: u64 = MM_REGION_SIZE * 8;
// This value is a fake and will be updated in a future PR
const CPI_SCRATCHPAD_ADDRESS: u64 = MM_REGION_SIZE * 255;

impl RuntimeGuestTransaction {
    pub fn new_with_feature_set(
        transaction_context: &TransactionContext,
        num_instructions: usize,
        feature_set: &SVMFeatureSet,
    ) -> Option<RuntimeGuestTransaction> {
        if feature_set.enable_abi_v2_programs {
            return Some(Self::new(transaction_context, num_instructions));
        }

        None
    }

    pub(crate) fn new(
        transaction_context: &TransactionContext,
        num_instructions: usize,
    ) -> RuntimeGuestTransaction {
        let size = size_of::<GuestTransactionContext>().saturating_add(
            (transaction_context.get_number_of_accounts() as usize)
                .saturating_mul(size_of::<GuestTransactionAccount>()),
        );
        let mut memory_vec: Vec<u8> = Vec::with_capacity(size);
        let memory = memory_vec.spare_capacity_mut();

        // SAFETY: The memory region is large enough to contain a GuestTransactionContext
        let guest_transaction =
            unsafe { &mut *(memory.as_mut_ptr() as *mut GuestTransactionContext) };

        let scratchpad = transaction_context.get_return_data();

        guest_transaction.return_data_scratchpad = ReturnDataScratchpad {
            pubkey: *scratchpad.0,
            slice: GuestSliceReference {
                pointer: RETURN_DATA_SCRATCHPAD_ADDRESS,
                length: scratchpad.1.len() as u64,
            },
        };

        guest_transaction.cpi_scratchpad = GuestSliceReference {
            pointer: CPI_SCRATCHPAD_ADDRESS,
            length: 0,
        };

        guest_transaction.instruction_idx = 0;
        guest_transaction.instruction_num = num_instructions as u64;
        guest_transaction.accounts_no = transaction_context.get_number_of_accounts() as u64;

        // SAFETY: The memory region is large enough to contain a GuestTransactionContext and an
        // array of `GuestTransactionAccount`
        let transaction_accounts = unsafe {
            let ptr = memory
                .as_mut_ptr()
                .add(size_of::<GuestTransactionContext>());
            slice::from_raw_parts_mut(
                ptr as *mut GuestTransactionAccount,
                guest_transaction.accounts_no as usize,
            )
        };

        for index_in_transaction in 0..transaction_context.get_number_of_accounts() {
            let transaction_account = transaction_context
                .accounts()
                .try_borrow(index_in_transaction)
                .unwrap();

            let account_ref = transaction_accounts
                .get_mut(index_in_transaction as usize)
                .unwrap();
            account_ref.pubkey = *transaction_context
                .get_key_of_account_at_index(index_in_transaction)
                .unwrap();
            account_ref.owner = *transaction_account.owner();
            account_ref.lamports = transaction_account.lamports();
            let vm_data_addr = ACCOUNT_DATA_VM_ADDRESS
                .saturating_add(MM_REGION_SIZE.saturating_mul(index_in_transaction as u64));
            account_ref.data = GuestSliceReference {
                pointer: vm_data_addr,
                length: transaction_account.data().len() as u64,
            };
        }

        // SAFETY: The vector has been allocated with at least `size` bytes.
        unsafe {
            memory_vec.set_len(size);
        }

        RuntimeGuestTransaction {
            data: memory_vec.into_boxed_slice(),
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn set_instruction_index(&mut self, index: usize) {
        // SAFETY: We assume the transaction was created using `RuntimeGuestTransaction::new`, which
        // guarantees the safety of size constraints and contents.
        let context = unsafe { &mut *(self.data.as_mut_ptr() as *mut GuestTransactionContext) };

        context.instruction_idx = index as u64;
    }
}

#[cfg(test)]
mod test {
    use {
        crate::guest_transaction::{
            GuestTransactionAccount, GuestTransactionContext, RuntimeGuestTransaction,
            ACCOUNT_DATA_VM_ADDRESS, CPI_SCRATCHPAD_ADDRESS, RETURN_DATA_SCRATCHPAD_ADDRESS,
        },
        solana_account::{Account, AccountSharedData, ReadableAccount},
        solana_rent::Rent,
        solana_sbpf::ebpf::MM_REGION_SIZE,
        solana_sdk_ids::bpf_loader,
        solana_transaction_context::TransactionContext,
        std::slice,
    };

    #[test]
    fn test_creation() {
        let transaction_accounts = vec![
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 0,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 0,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 1,
                    data: vec![1u8, 2, 3, 4, 5],
                    owner: bpf_loader::id(),
                    executable: false,
                    rent_epoch: 100,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 2,
                    data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 200,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 3,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: false,
                    rent_epoch: 300,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 4,
                    data: vec![1u8, 2, 3, 4, 5],
                    owner: bpf_loader::id(),
                    executable: false,
                    rent_epoch: 100,
                }),
            ),
            (
                solana_pubkey::new_rand(),
                AccountSharedData::from(Account {
                    lamports: 5,
                    data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 200,
                }),
            ),
        ];
        let mut context = TransactionContext::new(transaction_accounts, Rent::default(), 256, 256);
        let return_data_key = solana_pubkey::new_rand();
        let data = vec![1u8, 2, 3, 4, 5];
        context.set_return_data(return_data_key, data).unwrap();

        let mut runtime_transaction = RuntimeGuestTransaction::new(&context, 28);

        let guest_transaction = unsafe {
            &*(runtime_transaction.as_slice().as_ptr() as *const GuestTransactionContext)
        };

        assert_eq!(
            guest_transaction.return_data_scratchpad.pubkey,
            return_data_key
        );
        assert_eq!(
            guest_transaction.return_data_scratchpad.slice.pointer,
            RETURN_DATA_SCRATCHPAD_ADDRESS
        );
        assert_eq!(guest_transaction.return_data_scratchpad.slice.length, 5);

        assert_eq!(
            guest_transaction.cpi_scratchpad.pointer,
            CPI_SCRATCHPAD_ADDRESS
        );
        assert_eq!(guest_transaction.cpi_scratchpad.length, 0);

        assert_eq!(guest_transaction.instruction_idx, 0);
        assert_eq!(guest_transaction.instruction_num, 28);
        runtime_transaction.set_instruction_index(80);
        assert_eq!(guest_transaction.instruction_idx, 80);

        assert_eq!(guest_transaction.accounts_no, 6);

        let guest_accounts = unsafe {
            let ptr = runtime_transaction
                .as_slice()
                .as_ptr()
                .add(size_of::<GuestTransactionContext>());
            slice::from_raw_parts(
                ptr as *const GuestTransactionAccount,
                guest_transaction.accounts_no as usize,
            )
        };

        for i in 0..context.get_number_of_accounts() {
            let guest_account = guest_accounts.get(i as usize).unwrap();
            let tx_account = context.accounts().try_borrow(i).unwrap();

            assert_eq!(
                *context.get_key_of_account_at_index(i).unwrap(),
                guest_account.pubkey
            );
            assert_eq!(*tx_account.owner(), guest_account.owner);
            assert_eq!(tx_account.lamports(), guest_account.lamports);
            let addr = ACCOUNT_DATA_VM_ADDRESS + MM_REGION_SIZE * i as u64;
            assert_eq!(addr, guest_account.data.pointer);
            assert_eq!(tx_account.data().len() as u64, guest_account.data.length);
        }
    }
}
