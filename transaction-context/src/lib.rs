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
    crate::transaction_accounts::{AccountRefMut, KeyedAccountSharedData, TransactionAccounts},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_instruction::error::InstructionError,
    solana_instructions_sysvar as instructions,
    solana_pubkey::Pubkey,
    solana_sbpf::memory_region::{AccessType, AccessViolationHandler, MemoryRegion},
    std::{borrow::Cow, cell::Cell, collections::HashSet, rc::Rc},
};
#[cfg(not(target_os = "solana"))]
use {solana_account::WritableAccount, solana_rent::Rent};

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

/// Contains account meta data which varies between instruction.
///
/// It also contains indices to other structures for faster lookup.
///
/// This data structure is supposed to be shared with programs in ABIv2, so do not modify it
/// without consulting SIMD-0177.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct InstructionAccount {
    /// Points to the account and its key in the `TransactionContext`
    pub index_in_transaction: IndexOfAccount,
    /// Is this account supposed to sign
    is_signer: u8,
    /// Is this account allowed to become writable
    is_writable: u8,
}

impl InstructionAccount {
    pub fn new(
        index_in_transaction: IndexOfAccount,
        is_signer: bool,
        is_writable: bool,
    ) -> InstructionAccount {
        InstructionAccount {
            index_in_transaction,
            is_signer: is_signer as u8,
            is_writable: is_writable as u8,
        }
    }

    pub fn is_signer(&self) -> bool {
        self.is_signer != 0
    }

    pub fn is_writable(&self) -> bool {
        self.is_writable != 0
    }

    pub fn set_is_signer(&mut self, value: bool) {
        self.is_signer = value as u8;
    }

    pub fn set_is_writable(&mut self, value: bool) {
        self.is_writable = value as u8;
    }
}

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
        deduplication_map: Vec<u16>,
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
        debug_assert!(instruction_accounts.len() <= u16::MAX as usize);
        let mut dedup_map = vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION];
        for (idx, account) in instruction_accounts.iter().enumerate() {
            let index_in_instruction = dedup_map
                .get_mut(account.index_in_transaction as usize)
                .unwrap();
            if *index_in_instruction == u16::MAX {
                *index_in_instruction = idx as u16;
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

/// Instruction shared between runtime and programs.
#[derive(Debug, Clone, Default)]
pub struct InstructionFrame<'ix_data> {
    pub nesting_level: usize,
    pub program_account_index_in_tx: IndexOfAccount,
    pub instruction_accounts: Vec<InstructionAccount>,
    /// This is an account deduplication map that maps index_in_transaction to index_in_instruction
    /// Usage: dedup_map[index_in_transaction] = index_in_instruction
    /// This is a vector of u8s to save memory, since many entries may be unused.
    dedup_map: Vec<u16>,
    pub instruction_data: Cow<'ix_data, [u8]>,
}

/// View interface to read instructions.
#[derive(Debug, Clone)]
pub struct InstructionContext<'a, 'ix_data> {
    transaction_context: &'a TransactionContext<'ix_data>,
    // The rest of the fields are redundant shortcuts
    index_in_trace: usize,
    nesting_level: usize,
    program_account_index_in_tx: IndexOfAccount,
    instruction_accounts: &'a [InstructionAccount],
    dedup_map: &'a [u16],
    instruction_data: &'ix_data [u8],
}

impl<'a> InstructionContext<'a, '_> {
    /// How many Instructions were on the trace before this one was pushed
    pub fn get_index_in_trace(&self) -> usize {
        self.index_in_trace
    }

    /// How many Instructions were on the stack after this one was pushed
    ///
    /// That is the number of nested parent Instructions plus one (itself).
    pub fn get_stack_height(&self) -> usize {
        self.nesting_level.saturating_add(1)
    }

    /// Number of accounts in this Instruction (without program accounts)
    pub fn get_number_of_instruction_accounts(&self) -> IndexOfAccount {
        self.instruction_accounts.len() as IndexOfAccount
    }

    /// Assert that enough accounts were supplied to this Instruction
    pub fn check_number_of_instruction_accounts(
        &self,
        expected_at_least: IndexOfAccount,
    ) -> Result<(), InstructionError> {
        if self.get_number_of_instruction_accounts() < expected_at_least {
            Err(InstructionError::MissingAccount)
        } else {
            Ok(())
        }
    }

    /// Data parameter for the programs `process_instruction` handler
    pub fn get_instruction_data(&self) -> &[u8] {
        self.instruction_data
    }

    /// Translates the given instruction wide program_account_index into a transaction wide index
    pub fn get_index_of_program_account_in_transaction(
        &self,
    ) -> Result<IndexOfAccount, InstructionError> {
        if self.program_account_index_in_tx == u16::MAX {
            Err(InstructionError::MissingAccount)
        } else {
            Ok(self.program_account_index_in_tx)
        }
    }

    /// Translates the given instruction wide instruction_account_index into a transaction wide index
    pub fn get_index_of_instruction_account_in_transaction(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<IndexOfAccount, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .index_in_transaction as IndexOfAccount)
    }

    /// Get the index of account in instruction from the index in transaction
    pub fn get_index_of_account_in_instruction(
        &self,
        index_in_transaction: IndexOfAccount,
    ) -> Result<IndexOfAccount, InstructionError> {
        self.dedup_map
            .get(index_in_transaction as usize)
            .and_then(|idx| {
                if *idx as usize >= self.instruction_accounts.len() {
                    None
                } else {
                    Some(*idx as IndexOfAccount)
                }
            })
            .ok_or(InstructionError::MissingAccount)
    }

    /// Returns `Some(instruction_account_index)` if this is a duplicate
    /// and `None` if it is the first account with this key
    pub fn is_instruction_account_duplicate(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Option<IndexOfAccount>, InstructionError> {
        let index_in_transaction =
            self.get_index_of_instruction_account_in_transaction(instruction_account_index)?;
        let first_instruction_account_index =
            self.get_index_of_account_in_instruction(index_in_transaction)?;

        Ok(
            if first_instruction_account_index == instruction_account_index {
                None
            } else {
                Some(first_instruction_account_index)
            },
        )
    }

    /// Gets the key of the last program account of this Instruction
    pub fn get_program_key(&self) -> Result<&'a Pubkey, InstructionError> {
        self.get_index_of_program_account_in_transaction()
            .and_then(|index_in_transaction| {
                self.transaction_context
                    .get_key_of_account_at_index(index_in_transaction)
            })
    }

    /// Get the owner of the program account of this instruction
    pub fn get_program_owner(&self) -> Result<Pubkey, InstructionError> {
        self.get_index_of_program_account_in_transaction()
            .and_then(|index_in_transaction| {
                self.transaction_context
                    .accounts
                    .try_borrow(index_in_transaction)
            })
            .map(|acc| *acc.owner())
    }

    /// Gets an instruction account of this Instruction
    pub fn try_borrow_instruction_account(
        &self,
        index_in_instruction: IndexOfAccount,
    ) -> Result<BorrowedInstructionAccount<'_, '_>, InstructionError> {
        let instruction_account = *self
            .instruction_accounts
            .get(index_in_instruction as usize)
            .ok_or(InstructionError::MissingAccount)?;

        let account = self
            .transaction_context
            .accounts
            .try_borrow_mut(instruction_account.index_in_transaction)?;

        Ok(BorrowedInstructionAccount {
            transaction_context: self.transaction_context,
            instruction_account,
            account,
            index_in_transaction_of_instruction_program: self.program_account_index_in_tx,
        })
    }

    /// Returns whether an instruction account is a signer
    pub fn is_instruction_account_signer(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<bool, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .is_signer())
    }

    /// Returns whether an instruction account is writable
    pub fn is_instruction_account_writable(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<bool, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .is_writable())
    }

    /// Calculates the set of all keys of signer instruction accounts in this Instruction
    pub fn get_signers(&self) -> Result<HashSet<Pubkey>, InstructionError> {
        let mut result = HashSet::new();
        for instruction_account in self.instruction_accounts.iter() {
            if instruction_account.is_signer() {
                result.insert(
                    *self
                        .transaction_context
                        .get_key_of_account_at_index(instruction_account.index_in_transaction)?,
                );
            }
        }
        Ok(result)
    }

    pub fn instruction_accounts(&self) -> &[InstructionAccount] {
        self.instruction_accounts
    }

    pub fn get_key_of_instruction_account(
        &self,
        index_in_instruction: IndexOfAccount,
    ) -> Result<&'a Pubkey, InstructionError> {
        self.get_index_of_instruction_account_in_transaction(index_in_instruction)
            .and_then(|idx| self.transaction_context.get_key_of_account_at_index(idx))
    }
}

/// Shared account borrowed from the TransactionContext and an InstructionContext.
#[derive(Debug)]
pub struct BorrowedInstructionAccount<'a, 'ix_data> {
    transaction_context: &'a TransactionContext<'ix_data>,
    account: AccountRefMut<'a>,
    instruction_account: InstructionAccount,
    index_in_transaction_of_instruction_program: IndexOfAccount,
}

impl BorrowedInstructionAccount<'_, '_> {
    /// Returns the index of this account (transaction wide)
    #[inline]
    pub fn get_index_in_transaction(&self) -> IndexOfAccount {
        self.instruction_account.index_in_transaction
    }

    /// Returns the public key of this account (transaction wide)
    #[inline]
    pub fn get_key(&self) -> &Pubkey {
        self.transaction_context
            .get_key_of_account_at_index(self.instruction_account.index_in_transaction)
            .unwrap()
    }

    /// Returns the owner of this account (transaction wide)
    #[inline]
    pub fn get_owner(&self) -> &Pubkey {
        self.account.owner()
    }

    /// Assignes the owner of this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn set_owner(&mut self, pubkey: &[u8]) -> Result<(), InstructionError> {
        // Only the owner can assign a new owner
        if !self.is_owned_by_current_program() {
            return Err(InstructionError::ModifiedProgramId);
        }
        // and only if the account is writable
        if !self.is_writable() {
            return Err(InstructionError::ModifiedProgramId);
        }
        // and only if the data is zero-initialized or empty
        if !is_zeroed(self.get_data()) {
            return Err(InstructionError::ModifiedProgramId);
        }
        // don't touch the account if the owner does not change
        if self.get_owner().to_bytes() == pubkey {
            return Ok(());
        }
        self.touch()?;
        self.account.copy_into_owner_from_slice(pubkey);
        Ok(())
    }

    /// Returns the number of lamports of this account (transaction wide)
    #[inline]
    pub fn get_lamports(&self) -> u64 {
        self.account.lamports()
    }

    /// Overwrites the number of lamports of this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn set_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        // An account not owned by the program cannot have its balance decrease
        if !self.is_owned_by_current_program() && lamports < self.get_lamports() {
            return Err(InstructionError::ExternalAccountLamportSpend);
        }
        // The balance of read-only may not change
        if !self.is_writable() {
            return Err(InstructionError::ReadonlyLamportChange);
        }
        // don't touch the account if the lamports do not change
        let old_lamports = self.get_lamports();
        if old_lamports == lamports {
            return Ok(());
        }

        let lamports_balance = (lamports as i128).saturating_sub(old_lamports as i128);
        self.transaction_context
            .accounts
            .add_lamports_delta(lamports_balance)?;

        self.touch()?;
        self.account.set_lamports(lamports);
        Ok(())
    }

    /// Adds lamports to this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn checked_add_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        self.set_lamports(
            self.get_lamports()
                .checked_add(lamports)
                .ok_or(InstructionError::ArithmeticOverflow)?,
        )
    }

    /// Subtracts lamports from this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn checked_sub_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        self.set_lamports(
            self.get_lamports()
                .checked_sub(lamports)
                .ok_or(InstructionError::ArithmeticOverflow)?,
        )
    }

    /// Returns a read-only slice of the account data (transaction wide)
    #[inline]
    pub fn get_data(&self) -> &[u8] {
        self.account.data()
    }

    /// Returns a writable slice of the account data (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn get_data_mut(&mut self) -> Result<&mut [u8], InstructionError> {
        self.can_data_be_changed()?;
        self.touch()?;
        self.make_data_mut();
        Ok(self.account.data_as_mut_slice())
    }

    /// Overwrites the account data and size (transaction wide).
    ///
    /// Call this when you have a slice of data you do not own and want to
    /// replace the account data with it.
    #[cfg(not(target_os = "solana"))]
    pub fn set_data_from_slice(&mut self, data: &[u8]) -> Result<(), InstructionError> {
        self.can_data_be_resized(data.len())?;
        self.touch()?;
        self.update_accounts_resize_delta(data.len())?;
        // Note that we intentionally don't call self.make_data_mut() here.  make_data_mut() will
        // allocate + memcpy the current data if self.account is shared. We don't need the memcpy
        // here tho because account.set_data_from_slice(data) is going to replace the content
        // anyway.
        self.account.set_data_from_slice(data);

        Ok(())
    }

    /// Resizes the account data (transaction wide)
    ///
    /// Fills it with zeros at the end if is extended or truncates at the end otherwise.
    #[cfg(not(target_os = "solana"))]
    pub fn set_data_length(&mut self, new_length: usize) -> Result<(), InstructionError> {
        self.can_data_be_resized(new_length)?;
        // don't touch the account if the length does not change
        if self.get_data().len() == new_length {
            return Ok(());
        }
        self.touch()?;
        self.update_accounts_resize_delta(new_length)?;
        self.account.resize(new_length, 0);
        Ok(())
    }

    /// Appends all elements in a slice to the account
    #[cfg(not(target_os = "solana"))]
    pub fn extend_from_slice(&mut self, data: &[u8]) -> Result<(), InstructionError> {
        let new_len = self.get_data().len().saturating_add(data.len());
        self.can_data_be_resized(new_len)?;

        if data.is_empty() {
            return Ok(());
        }

        self.touch()?;
        self.update_accounts_resize_delta(new_len)?;
        // Even if extend_from_slice never reduces capacity, still realloc using
        // make_data_mut() if necessary so that we grow the account of the full
        // max realloc length in one go, avoiding smaller reallocations.
        self.make_data_mut();
        self.account.extend_from_slice(data);
        Ok(())
    }

    /// Returns whether the underlying AccountSharedData is shared.
    ///
    /// The data is shared if the account has been loaded from the accounts database and has never
    /// been written to. Writing to an account unshares it.
    ///
    /// During account serialization, if an account is shared it'll get mapped as CoW, else it'll
    /// get mapped directly as writable.
    #[cfg(not(target_os = "solana"))]
    pub fn is_shared(&self) -> bool {
        self.account.is_shared()
    }

    #[cfg(not(target_os = "solana"))]
    fn make_data_mut(&mut self) {
        // if the account is still shared, it means this is the first time we're
        // about to write into it. Make the account mutable by copying it in a
        // buffer with MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION capacity so that if the
        // transaction reallocs, we don't have to copy the whole account data a
        // second time to fullfill the realloc.
        if self.account.is_shared() {
            self.account
                .reserve(MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION);
        }
    }

    /// Deserializes the account data into a state
    #[cfg(all(not(target_os = "solana"), feature = "bincode"))]
    pub fn get_state<T: serde::de::DeserializeOwned>(&self) -> Result<T, InstructionError> {
        bincode::deserialize(self.account.data()).map_err(|_| InstructionError::InvalidAccountData)
    }

    /// Serializes a state into the account data
    #[cfg(all(not(target_os = "solana"), feature = "bincode"))]
    pub fn set_state<T: serde::Serialize>(&mut self, state: &T) -> Result<(), InstructionError> {
        let data = self.get_data_mut()?;
        let serialized_size =
            bincode::serialized_size(state).map_err(|_| InstructionError::GenericError)?;
        if serialized_size > data.len() as u64 {
            return Err(InstructionError::AccountDataTooSmall);
        }
        bincode::serialize_into(&mut *data, state).map_err(|_| InstructionError::GenericError)?;
        Ok(())
    }

    // Returns whether or the lamports currently in the account is sufficient for rent exemption should the
    // data be resized to the given size
    #[cfg(not(target_os = "solana"))]
    pub fn is_rent_exempt_at_data_length(&self, data_length: usize) -> bool {
        self.transaction_context
            .rent
            .is_exempt(self.get_lamports(), data_length)
    }

    /// Returns whether this account is executable (transaction wide)
    #[inline]
    #[deprecated(since = "2.1.0", note = "Use `get_owner` instead")]
    pub fn is_executable(&self) -> bool {
        #[allow(deprecated)]
        self.account.executable()
    }

    /// Configures whether this account is executable (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn set_executable(&mut self, is_executable: bool) -> Result<(), InstructionError> {
        // To become executable an account must be rent exempt
        if !self
            .transaction_context
            .rent
            .is_exempt(self.get_lamports(), self.get_data().len())
        {
            return Err(InstructionError::ExecutableAccountNotRentExempt);
        }
        // Only the owner can set the executable flag
        if !self.is_owned_by_current_program() {
            return Err(InstructionError::ExecutableModified);
        }
        // and only if the account is writable
        if !self.is_writable() {
            return Err(InstructionError::ExecutableModified);
        }
        // don't touch the account if the executable flag does not change
        #[allow(deprecated)]
        if self.is_executable() == is_executable {
            return Ok(());
        }
        self.touch()?;
        self.account.set_executable(is_executable);
        Ok(())
    }

    /// Returns the rent epoch of this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    #[inline]
    pub fn get_rent_epoch(&self) -> u64 {
        self.account.rent_epoch()
    }

    /// Returns whether this account is a signer (instruction wide)
    pub fn is_signer(&self) -> bool {
        self.instruction_account.is_signer()
    }

    /// Returns whether this account is writable (instruction wide)
    pub fn is_writable(&self) -> bool {
        self.instruction_account.is_writable()
    }

    /// Returns true if the owner of this account is the current `InstructionContext`s last program (instruction wide)
    pub fn is_owned_by_current_program(&self) -> bool {
        self.transaction_context
            .get_key_of_account_at_index(self.index_in_transaction_of_instruction_program)
            .map(|program_key| program_key == self.get_owner())
            .unwrap_or_default()
    }

    /// Returns an error if the account data can not be mutated by the current program
    #[cfg(not(target_os = "solana"))]
    pub fn can_data_be_changed(&self) -> Result<(), InstructionError> {
        // and only if the account is writable
        if !self.is_writable() {
            return Err(InstructionError::ReadonlyDataModified);
        }
        // and only if we are the owner
        if !self.is_owned_by_current_program() {
            return Err(InstructionError::ExternalAccountDataModified);
        }
        Ok(())
    }

    /// Returns an error if the account data can not be resized to the given length
    #[cfg(not(target_os = "solana"))]
    pub fn can_data_be_resized(&self, new_len: usize) -> Result<(), InstructionError> {
        let old_len = self.get_data().len();
        // Only the owner can change the length of the data
        if new_len != old_len && !self.is_owned_by_current_program() {
            return Err(InstructionError::AccountDataSizeChanged);
        }
        self.transaction_context
            .accounts
            .can_data_be_resized(old_len, new_len)?;
        self.can_data_be_changed()
    }

    #[cfg(not(target_os = "solana"))]
    fn touch(&self) -> Result<(), InstructionError> {
        self.transaction_context
            .accounts
            .touch(self.instruction_account.index_in_transaction)
    }

    #[cfg(not(target_os = "solana"))]
    fn update_accounts_resize_delta(&mut self, new_len: usize) -> Result<(), InstructionError> {
        self.transaction_context
            .accounts
            .update_accounts_resize_delta(self.get_data().len(), new_len)
    }
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

#[cfg(not(target_os = "solana"))]
fn is_zeroed(buf: &[u8]) -> bool {
    const ZEROS_LEN: usize = 1024;
    const ZEROS: [u8; ZEROS_LEN] = [0; ZEROS_LEN];
    let mut chunks = buf.chunks_exact(ZEROS_LEN);

    #[allow(clippy::indexing_slicing)]
    {
        chunks.all(|chunk| chunk == &ZEROS[..])
            && chunks.remainder() == &ZEROS[..chunks.remainder().len()]
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
