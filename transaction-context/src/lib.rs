#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
//! Data shared between program runtime and built-in programs as well as SBF programs.
#![deny(clippy::indexing_slicing)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

use {
    crate::{
        instruction::{InstructionContext, InstructionFrame},
        instruction_accounts::InstructionAccount,
        transaction_accounts::{KeyedAccountSharedData, TransactionAccounts},
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_instruction::error::InstructionError,
    solana_instructions_sysvar as instructions,
    solana_pubkey::Pubkey,
    solana_sbpf::memory_region::{AccessType, AccessViolationHandler, MemoryRegion},
    std::{borrow::Cow, cell::Cell, rc::Rc},
};
#[cfg(not(target_os = "solana"))]
use {solana_account::WritableAccount, solana_rent::Rent};

pub mod instruction;
pub mod instruction_accounts;
pub mod transaction_accounts;
pub mod vm_slice;

pub const MAX_ACCOUNTS_PER_TRANSACTION: usize = 256;
// This is one less than MAX_ACCOUNTS_PER_TRANSACTION because
// one index is used as NON_DUP_MARKER in ABI v0 and v1.
pub const MAX_ACCOUNTS_PER_INSTRUCTION: usize = 255;
pub const MAX_INSTRUCTION_DATA_LEN: usize = 10 * 1024;
pub const MAX_ACCOUNT_DATA_LEN: u64 = 10 * 1024 * 1024;
// Note: With stricter_abi_and_runtime_constraints programs can grow accounts
// faster than they intend to, because the AccessViolationHandler might grow
// an account up to MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION at once.
pub const MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION: i64 = MAX_ACCOUNT_DATA_LEN as i64 * 2;
pub const MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION: usize = 10 * 1_024;
// Maximum cross-program invocation and instructions per transaction
pub const MAX_INSTRUCTION_TRACE_LENGTH: usize = 64;

#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNTS_PER_INSTRUCTION,
    solana_program_entrypoint::NON_DUP_MARKER as usize,
);
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNT_DATA_LEN,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
);
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION,
    solana_system_interface::MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
);
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION,
    solana_account_info::MAX_PERMITTED_DATA_INCREASE,
);

/// Index of an account inside of the transaction or an instruction.
pub type IndexOfAccount = u16;

/// Loaded transaction shared between runtime and programs.
///
/// This context is valid for the entire duration of a transaction being processed.
#[derive(Debug)]
pub struct TransactionContext<'ix_data> {
    accounts: Rc<TransactionAccounts>,
    instruction_stack_capacity: usize,
    instruction_trace_capacity: usize,
    instruction_stack: Vec<usize>,
    instruction_trace: Vec<InstructionFrame<'ix_data>>,
    top_level_instruction_index: usize,
    return_data: TransactionReturnData,
    #[cfg(not(target_os = "solana"))]
    rent: Rent,
}

impl<'ix_data> TransactionContext<'ix_data> {
    /// Constructs a new TransactionContext
    #[cfg(not(target_os = "solana"))]
    pub fn new(
        transaction_accounts: Vec<KeyedAccountSharedData>,
        rent: Rent,
        instruction_stack_capacity: usize,
        instruction_trace_capacity: usize,
    ) -> Self {
        Self {
            accounts: Rc::new(TransactionAccounts::new(transaction_accounts)),
            instruction_stack_capacity,
            instruction_trace_capacity,
            instruction_stack: Vec::with_capacity(instruction_stack_capacity),
            instruction_trace: vec![InstructionFrame::default()],
            top_level_instruction_index: 0,
            return_data: TransactionReturnData::default(),
            rent,
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn set_top_level_instruction_index(&mut self, top_level_instruction_index: usize) {
        self.top_level_instruction_index = top_level_instruction_index;
    }

    /// Used in mock_process_instruction
    #[cfg(not(target_os = "solana"))]
    pub fn deconstruct_without_keys(self) -> Result<Vec<AccountSharedData>, InstructionError> {
        if !self.instruction_stack.is_empty() {
            return Err(InstructionError::CallDepth);
        }

        let accounts = Rc::try_unwrap(self.accounts)
            .expect("transaction_context.accounts has unexpected outstanding refs")
            .deconstruct_into_account_shared_data();

        Ok(accounts)
    }

    #[cfg(not(target_os = "solana"))]
    pub fn accounts(&self) -> &Rc<TransactionAccounts> {
        &self.accounts
    }

    /// Returns the total number of accounts loaded in this Transaction
    pub fn get_number_of_accounts(&self) -> IndexOfAccount {
        self.accounts.len() as IndexOfAccount
    }

    /// Searches for an account by its key
    pub fn get_key_of_account_at_index(
        &self,
        index_in_transaction: IndexOfAccount,
    ) -> Result<&Pubkey, InstructionError> {
        self.accounts
            .account_key(index_in_transaction)
            .ok_or(InstructionError::MissingAccount)
    }

    /// Searches for an account by its key
    pub fn find_index_of_account(&self, pubkey: &Pubkey) -> Option<IndexOfAccount> {
        self.accounts
            .account_keys_iter()
            .position(|key| key == pubkey)
            .map(|index| index as IndexOfAccount)
    }

    /// Gets the max length of the instruction trace
    pub fn get_instruction_trace_capacity(&self) -> usize {
        self.instruction_trace_capacity
    }

    /// Returns the instruction trace length.
    ///
    /// Not counting the last empty instruction which is always pre-reserved for the next instruction.
    /// See also `get_next_instruction_context()`.
    pub fn get_instruction_trace_length(&self) -> usize {
        self.instruction_trace.len().saturating_sub(1)
    }

    /// Gets a view on an instruction by its index in the trace
    pub fn get_instruction_context_at_index_in_trace(
        &self,
        index_in_trace: usize,
    ) -> Result<InstructionContext<'_, '_>, InstructionError> {
        let instruction = self
            .instruction_trace
            .get(index_in_trace)
            .ok_or(InstructionError::CallDepth)?;
        Ok(InstructionContext {
            transaction_context: self,
            index_in_trace,
            nesting_level: instruction.nesting_level,
            program_account_index_in_tx: instruction.program_account_index_in_tx,
            instruction_accounts: &instruction.instruction_accounts,
            dedup_map: &instruction.dedup_map,
            instruction_data: &instruction.instruction_data,
        })
    }

    /// Gets a view on the instruction by its nesting level in the stack
    pub fn get_instruction_context_at_nesting_level(
        &self,
        nesting_level: usize,
    ) -> Result<InstructionContext<'_, '_>, InstructionError> {
        let index_in_trace = *self
            .instruction_stack
            .get(nesting_level)
            .ok_or(InstructionError::CallDepth)?;
        let instruction_context = self.get_instruction_context_at_index_in_trace(index_in_trace)?;
        debug_assert_eq!(instruction_context.nesting_level, nesting_level);
        Ok(instruction_context)
    }

    /// Gets the max height of the instruction stack
    pub fn get_instruction_stack_capacity(&self) -> usize {
        self.instruction_stack_capacity
    }

    /// Gets instruction stack height, top-level instructions are height
    /// `solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT`
    pub fn get_instruction_stack_height(&self) -> usize {
        self.instruction_stack.len()
    }

    /// Returns a view on the current instruction
    pub fn get_current_instruction_context(
        &self,
    ) -> Result<InstructionContext<'_, '_>, InstructionError> {
        let level = self
            .get_instruction_stack_height()
            .checked_sub(1)
            .ok_or(InstructionError::CallDepth)?;
        self.get_instruction_context_at_nesting_level(level)
    }

    /// Returns a view on the next instruction. This function assumes it has already been
    /// configured with the correct values in `prepare_next_instruction` or
    /// `prepare_next_top_level_instruction`
    pub fn get_next_instruction_context(
        &self,
    ) -> Result<InstructionContext<'_, '_>, InstructionError> {
        let index_in_trace = self
            .instruction_trace
            .len()
            .checked_sub(1)
            .ok_or(InstructionError::CallDepth)?;
        self.get_instruction_context_at_index_in_trace(index_in_trace)
    }

    /// Configures the next instruction.
    ///
    /// The last InstructionContext is always empty and pre-reserved for the next instruction.
    pub fn configure_next_instruction(
        &mut self,
        program_index: IndexOfAccount,
        instruction_accounts: Vec<InstructionAccount>,
        deduplication_map: Vec<u8>,
        instruction_data: Cow<'ix_data, [u8]>,
    ) -> Result<(), InstructionError> {
        debug_assert_eq!(deduplication_map.len(), MAX_ACCOUNTS_PER_TRANSACTION);
        let instruction = self
            .instruction_trace
            .last_mut()
            .ok_or(InstructionError::CallDepth)?;
        instruction.program_account_index_in_tx = program_index;
        instruction.instruction_accounts = instruction_accounts;
        instruction.instruction_data = instruction_data;
        instruction.dedup_map = deduplication_map;
        Ok(())
    }

    /// A version of `configure_next_instruction` to help creating the deduplication map in tests
    pub fn configure_next_instruction_for_tests(
        &mut self,
        program_index: IndexOfAccount,
        instruction_accounts: Vec<InstructionAccount>,
        instruction_data: Vec<u8>,
    ) -> Result<(), InstructionError> {
        debug_assert!(instruction_accounts.len() <= u8::MAX as usize);
        let mut dedup_map = vec![u8::MAX; MAX_ACCOUNTS_PER_TRANSACTION];
        for (idx, account) in instruction_accounts.iter().enumerate() {
            let index_in_instruction = dedup_map
                .get_mut(account.index_in_transaction as usize)
                .unwrap();
            if *index_in_instruction == u8::MAX {
                *index_in_instruction = idx as u8;
            }
        }
        self.configure_next_instruction(
            program_index,
            instruction_accounts,
            dedup_map,
            Cow::Owned(instruction_data),
        )
    }

    /// Pushes the next instruction
    #[cfg(not(target_os = "solana"))]
    pub fn push(&mut self) -> Result<(), InstructionError> {
        let nesting_level = self.get_instruction_stack_height();
        if !self.instruction_stack.is_empty() && self.accounts.get_lamports_delta() != 0 {
            return Err(InstructionError::UnbalancedInstruction);
        }
        {
            let instruction = self
                .instruction_trace
                .last_mut()
                .ok_or(InstructionError::CallDepth)?;
            instruction.nesting_level = nesting_level;
        }
        let index_in_trace = self.get_instruction_trace_length();
        if index_in_trace >= self.instruction_trace_capacity {
            return Err(InstructionError::MaxInstructionTraceLengthExceeded);
        }
        self.instruction_trace.push(InstructionFrame::default());
        if nesting_level >= self.instruction_stack_capacity {
            return Err(InstructionError::CallDepth);
        }
        self.instruction_stack.push(index_in_trace);
        if let Some(index_in_transaction) = self.find_index_of_account(&instructions::id()) {
            let mut mut_account_ref = self.accounts.try_borrow_mut(index_in_transaction)?;
            if mut_account_ref.owner() != &solana_sdk_ids::sysvar::id() {
                return Err(InstructionError::InvalidAccountOwner);
            }
            instructions::store_current_index_checked(
                mut_account_ref.data_as_mut_slice(),
                self.top_level_instruction_index as u16,
            )?;
        }
        Ok(())
    }

    /// Pops the current instruction
    #[cfg(not(target_os = "solana"))]
    pub fn pop(&mut self) -> Result<(), InstructionError> {
        if self.instruction_stack.is_empty() {
            return Err(InstructionError::CallDepth);
        }
        // Verify (before we pop) that the total sum of all lamports in this instruction did not change
        let detected_an_unbalanced_instruction =
            self.get_current_instruction_context()
                .and_then(|instruction_context| {
                    // Verify all executable accounts have no outstanding refs
                    self.accounts
                        .try_borrow_mut(
                            instruction_context.get_index_of_program_account_in_transaction()?,
                        )
                        .map_err(|err| {
                            if err == InstructionError::AccountBorrowFailed {
                                InstructionError::AccountBorrowOutstanding
                            } else {
                                err
                            }
                        })?;
                    Ok(self.accounts.get_lamports_delta() != 0)
                });
        // Always pop, even if we `detected_an_unbalanced_instruction`
        self.instruction_stack.pop();
        if self.instruction_stack.is_empty() {
            self.top_level_instruction_index = self.top_level_instruction_index.saturating_add(1);
        }
        if detected_an_unbalanced_instruction? {
            Err(InstructionError::UnbalancedInstruction)
        } else {
            Ok(())
        }
    }

    /// Gets the return data of the current instruction or any above
    pub fn get_return_data(&self) -> (&Pubkey, &[u8]) {
        (&self.return_data.program_id, &self.return_data.data)
    }

    /// Set the return data of the current instruction
    pub fn set_return_data(
        &mut self,
        program_id: Pubkey,
        data: Vec<u8>,
    ) -> Result<(), InstructionError> {
        self.return_data = TransactionReturnData { program_id, data };
        Ok(())
    }

    /// Returns a new account data write access handler
    pub fn access_violation_handler(
        &self,
        stricter_abi_and_runtime_constraints: bool,
        account_data_direct_mapping: bool,
    ) -> AccessViolationHandler {
        let accounts = Rc::clone(&self.accounts);
        Box::new(
            move |region: &mut MemoryRegion,
                  address_space_reserved_for_account: u64,
                  access_type: AccessType,
                  vm_addr: u64,
                  len: u64| {
                if access_type == AccessType::Load {
                    return;
                }
                let Some(index_in_transaction) = region.access_violation_handler_payload else {
                    // This region is not a writable account.
                    return;
                };
                let requested_length =
                    vm_addr.saturating_add(len).saturating_sub(region.vm_addr) as usize;
                if requested_length > address_space_reserved_for_account as usize {
                    // Requested access goes further than the account region.
                    return;
                }

                // The four calls below can't really fail. If they fail because of a bug,
                // whatever is writing will trigger an EbpfError::AccessViolation like
                // if the region was readonly, and the transaction will fail gracefully.
                let Ok(mut account) = accounts.try_borrow_mut(index_in_transaction) else {
                    debug_assert!(false);
                    return;
                };
                if accounts.touch(index_in_transaction).is_err() {
                    debug_assert!(false);
                    return;
                }

                let remaining_allowed_growth = MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION
                    .saturating_sub(accounts.resize_delta())
                    .max(0) as usize;

                if requested_length > region.len as usize {
                    // Realloc immediately here to fit the requested access,
                    // then later in CPI or deserialization realloc again to the
                    // account length the program stored in AccountInfo.
                    let old_len = account.data().len();
                    let new_len = (address_space_reserved_for_account as usize)
                        .min(MAX_ACCOUNT_DATA_LEN as usize)
                        .min(old_len.saturating_add(remaining_allowed_growth));
                    // The last two min operations ensure the following:
                    debug_assert!(accounts.can_data_be_resized(old_len, new_len).is_ok());
                    if accounts
                        .update_accounts_resize_delta(old_len, new_len)
                        .is_err()
                    {
                        return;
                    }
                    account.resize(new_len, 0);
                    region.len = new_len as u64;
                }

                // Potentially unshare / make the account shared data unique (CoW logic).
                if stricter_abi_and_runtime_constraints && account_data_direct_mapping {
                    region.host_addr = account.data_as_mut_slice().as_mut_ptr() as u64;
                    region.writable = true;
                }
            },
        )
    }

    /// Take ownership of the instruction trace
    pub fn take_instruction_trace(&mut self) -> Vec<InstructionFrame<'_>> {
        // The last frame is a placeholder for the next instruction to be executed, so it
        // is empty.
        self.instruction_trace.pop();
        std::mem::take(&mut self.instruction_trace)
    }
}

/// Return data at the end of a transaction
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionReturnData {
    pub program_id: Pubkey,
    pub data: Vec<u8>,
}

/// Everything that needs to be recorded from a TransactionContext after execution
#[cfg(not(target_os = "solana"))]
pub struct ExecutionRecord {
    pub accounts: Vec<KeyedAccountSharedData>,
    pub return_data: TransactionReturnData,
    pub touched_account_count: u64,
    pub accounts_resize_delta: i64,
}

/// Used by the bank in the runtime to write back the processed accounts and recorded instructions
#[cfg(not(target_os = "solana"))]
impl From<TransactionContext<'_>> for ExecutionRecord {
    fn from(context: TransactionContext) -> Self {
        let (accounts, touched_flags, resize_delta) = Rc::try_unwrap(context.accounts)
            .expect("transaction_context.accounts has unexpected outstanding refs")
            .take();
        let touched_account_count = touched_flags
            .iter()
            .fold(0usize, |accumulator, was_touched| {
                accumulator.saturating_add(was_touched.get() as usize)
            }) as u64;
        Self {
            accounts,
            return_data: context.return_data,
            touched_account_count,
            accounts_resize_delta: Cell::into_inner(resize_delta),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instructions_sysvar_store_index_checked() {
        let build_transaction_context = |account: AccountSharedData| {
            TransactionContext::new(
                vec![
                    (Pubkey::new_unique(), AccountSharedData::default()),
                    (instructions::id(), account),
                ],
                Rent::default(),
                /* max_instruction_stack_depth */ 2,
                /* max_instruction_trace_length */ 2,
            )
        };

        let correct_space = 2;
        let rent_exempt_lamports = Rent::default().minimum_balance(correct_space);

        // First try it with the wrong owner.
        let account =
            AccountSharedData::new(rent_exempt_lamports, correct_space, &Pubkey::new_unique());
        assert_eq!(
            build_transaction_context(account).push(),
            Err(InstructionError::InvalidAccountOwner),
        );

        // Now with the wrong data length.
        let account =
            AccountSharedData::new(rent_exempt_lamports, 0, &solana_sdk_ids::sysvar::id());
        assert_eq!(
            build_transaction_context(account).push(),
            Err(InstructionError::AccountDataTooSmall),
        );

        // Finally provide the correct account setup.
        let account = AccountSharedData::new(
            rent_exempt_lamports,
            correct_space,
            &solana_sdk_ids::sysvar::id(),
        );
        assert_eq!(build_transaction_context(account).push(), Ok(()),);
    }

    #[test]
    fn test_invalid_native_loader_index() {
        let mut transaction_context = TransactionContext::new(
            vec![(
                Pubkey::new_unique(),
                AccountSharedData::new(1, 1, &Pubkey::new_unique()),
            )],
            Rent::default(),
            20,
            20,
        );

        transaction_context
            .configure_next_instruction_for_tests(
                u16::MAX,
                vec![InstructionAccount::new(0, false, false)],
                vec![],
            )
            .unwrap();
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();

        let result = instruction_context.get_index_of_program_account_in_transaction();
        assert_eq!(result, Err(InstructionError::MissingAccount));

        let result = instruction_context.get_program_key();
        assert_eq!(result, Err(InstructionError::MissingAccount));

        let result = instruction_context.get_program_owner();
        assert_eq!(result.err(), Some(InstructionError::MissingAccount));
    }
}
