#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
use {
    crate::{
        IndexOfAccount, MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION, MAX_ACCOUNT_DATA_LEN,
        MAX_ACCOUNTS_PER_TRANSACTION, MAX_INSTRUCTION_TRACE_LENGTH,
        instruction::{InstructionContext, InstructionFrame},
        transaction_accounts::{KeyedAccountSharedData, TransactionAccounts},
        vm_addresses::{
            GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS, GUEST_ACCOUNT_PAYLOAD_END_ADDRESS,
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS, GUEST_INSTRUCTION_ACCOUNT_END_ADDRESS,
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS, GUEST_INSTRUCTION_DATA_END_ADDRESS,
            GUEST_REGION_SIZE, INSTRUCTION_TRACE_AREA, RETURN_DATA_SCRATCHPAD,
            abiv2_region_index_from_vm_address,
        },
    },
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_instruction::error::InstructionError,
    solana_instructions_sysvar as instructions,
    solana_rent::Rent,
    solana_sbpf::memory_region::{AccessType, AccessViolationHandler, MemoryRegion, VmExposable},
    std::{borrow::Cow, cell::Cell, rc::Rc},
};
use {
    crate::{instruction_accounts::InstructionAccount, vm_slice::VmSlice},
    solana_pubkey::Pubkey,
};

/// Used only in fn `take_instruction_trace` for deconstructing TransactionContext
#[cfg(not(any(target_arch = "sbf", target_arch = "bpf")))]
pub type InstructionTrace<'ix_data> = (
    Vec<InstructionFrame>,
    Vec<Vec<InstructionAccount>>,
    Vec<Cow<'ix_data, [u8]>>,
);

/// This data structure is shared with programs in ABIv2, providing information about the
/// transaction metadata.
///
/// Modifications without a feature gate and proper versioning might break programs.
#[repr(C)]
#[derive(Debug)]
pub struct TransactionFrame {
    /// Pubkey of the last program to write to the return data scratchpad
    pub return_data_pubkey: Pubkey,
    pub return_data_scratchpad: VmSlice<u8>,
    /// Scratchpad for programs to write CPI instruction data
    pub cpi_data_scratchpad: VmSlice<u8>,
    /// Scratchpad for programs to write CPI accounts
    pub cpi_accounts_scratchpad: VmSlice<InstructionAccount>,
    /// Index of current executing instruction
    pub current_executing_instruction: u16,
    /// Number of instructions in the instruction trace (including top level and CPIs)
    pub total_number_of_instructions_in_trace: u16,
    /// Number of CPIs in the instruction trace
    pub number_of_cpis_in_trace: u16,
    /// Number of transaction accounts
    pub number_of_transaction_accounts: u16,
}

#[cfg(not(any(target_arch = "sbf", target_arch = "bpf")))]
impl VmExposable for TransactionFrame {}

#[cfg(not(any(target_arch = "sbf", target_arch = "bpf")))]
impl TransactionFrame {
    fn configure_cpi(&mut self) {
        self.total_number_of_instructions_in_trace =
            self.total_number_of_instructions_in_trace.saturating_add(1);
        let next_data_ptr = self
            .cpi_data_scratchpad
            .ptr()
            .saturating_add(GUEST_REGION_SIZE);
        self.cpi_data_scratchpad = VmSlice::new(next_data_ptr, 0);
        let next_accounts_ptr = self
            .cpi_accounts_scratchpad
            .ptr()
            .saturating_add(GUEST_REGION_SIZE);
        self.cpi_accounts_scratchpad = VmSlice::new(next_accounts_ptr, 0);
    }
}

/// Loaded transaction shared between runtime and programs.
///
/// This context is valid for the entire duration of a transaction being processed.
#[derive(Debug)]
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub struct TransactionContext<'ix_data> {
    pub(crate) accounts: Rc<TransactionAccounts>,
    instruction_stack_capacity: usize,
    instruction_trace_capacity: usize,
    instruction_stack: Vec<usize>,
    instruction_trace: Vec<InstructionFrame>,
    transaction_frame: TransactionFrame,
    return_data_bytes: Vec<u8>,
    next_top_level_instruction_index: usize,
    #[cfg(not(target_os = "solana"))]
    pub(crate) rent: Rent,
    /// This is an account deduplication map that maps index_in_transaction to index_in_instruction
    /// Usage: dedup_map[index_in_transaction] = index_in_instruction
    /// Each entry in `deduplication_maps` represents the deduplication map for each instruction.
    deduplication_maps: Vec<Box<[u16]>>,
    /// Each entry in `instruction_accounts` represents the array of accounts for each instruction.
    instruction_accounts: Vec<Vec<InstructionAccount>>,
    /// Each entry in `instruction_data` represents the data for instruction at the corresponding
    /// index.
    instruction_data: Vec<Cow<'ix_data, [u8]>>,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl<'ix_data> TransactionContext<'ix_data> {
    /// Constructs a new TransactionContext
    pub fn new(
        transaction_accounts: Vec<KeyedAccountSharedData>,
        rent: Rent,
        instruction_stack_capacity: usize,
        instruction_trace_capacity: usize,
        number_of_top_level_instructions: usize,
    ) -> Self {
        let transaction_frame = TransactionFrame {
            return_data_pubkey: Pubkey::default(),
            return_data_scratchpad: VmSlice::new(RETURN_DATA_SCRATCHPAD, 0),
            cpi_data_scratchpad: VmSlice::new(
                GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(
                    GUEST_REGION_SIZE.saturating_mul(number_of_top_level_instructions as u64),
                ),
                0,
            ),
            cpi_accounts_scratchpad: VmSlice::new(
                GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS.saturating_add(
                    GUEST_REGION_SIZE.saturating_mul(number_of_top_level_instructions as u64),
                ),
                0,
            ),
            current_executing_instruction: 0,
            total_number_of_instructions_in_trace: number_of_top_level_instructions as u16,
            number_of_cpis_in_trace: 0,
            number_of_transaction_accounts: transaction_accounts.len() as u16,
        };

        // We need an extra space for the placeholder, so we avoid relocations.
        let mut instruction_trace =
            Vec::with_capacity(instruction_trace_capacity.saturating_add(1));
        let mut deduplication_maps =
            Vec::with_capacity(instruction_trace_capacity.saturating_add(1));
        let mut instruction_accounts =
            Vec::with_capacity(instruction_trace_capacity.saturating_add(1));
        let mut instruction_data = Vec::with_capacity(instruction_trace_capacity.saturating_add(1));

        instruction_trace.resize_with(
            number_of_top_level_instructions.saturating_add(1),
            InstructionFrame::default,
        );
        deduplication_maps.resize_with(
            number_of_top_level_instructions.saturating_add(1),
            Box::default,
        );
        instruction_accounts
            .resize_with(number_of_top_level_instructions.saturating_add(1), Vec::new);
        instruction_data.resize(
            number_of_top_level_instructions.saturating_add(1),
            Cow::Owned(Vec::new()),
        );

        Self {
            accounts: Rc::new(TransactionAccounts::new(transaction_accounts)),
            instruction_stack_capacity,
            instruction_trace_capacity,
            instruction_stack: Vec::with_capacity(instruction_stack_capacity),
            instruction_trace,
            return_data_bytes: Vec::new(),
            transaction_frame,
            next_top_level_instruction_index: 0,
            rent,
            instruction_accounts,
            deduplication_maps,
            instruction_data,
        }
    }

    /// Used in mock_process_instruction
    pub fn deconstruct_without_keys(self) -> Result<Vec<AccountSharedData>, InstructionError> {
        if !self.instruction_stack.is_empty() {
            return Err(InstructionError::CallDepth);
        }

        let accounts = Rc::try_unwrap(self.accounts)
            .expect("transaction_context.accounts has unexpected outstanding refs")
            .deconstruct_into_account_shared_data();

        Ok(accounts)
    }

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
    pub fn get_instruction_trace_length(&self) -> usize {
        self.instruction_trace.len().saturating_sub(1)
    }

    /// Check if the provided instruction index is that of an
    pub fn is_upcoming_cpi_ix_index(&self, ix_idx: usize) -> bool {
        self.get_instruction_trace_length() == ix_idx
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

        // These commands will return a default empty slice if we are retrieving an instruction
        // that hasn't been configured yet.
        let instruction_accounts = self
            .instruction_accounts
            .get(index_in_trace)
            .map(|item| item.as_ref())
            .unwrap_or_default();
        let dedup_map = self
            .deduplication_maps
            .get(index_in_trace)
            .map(|item| item.as_ref())
            .unwrap_or_default();
        let instruction_data = self
            .instruction_data
            .get(index_in_trace)
            .map(|item| item.as_ref())
            .unwrap_or_default();
        Ok(InstructionContext {
            transaction_context: self,
            index_in_trace,
            nesting_level: instruction.nesting_level as usize,
            program_account_index_in_tx: instruction.program_account_index_in_tx as IndexOfAccount,
            instruction_accounts,
            dedup_map,
            instruction_data,
            index_of_caller_instruction: instruction.index_of_caller_instruction as usize,
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

    /// Returns the index in the instruction trace of the current executing instruction
    pub fn get_current_instruction_index(&self) -> Result<usize, InstructionError> {
        self.instruction_stack
            .last()
            .copied()
            .ok_or(InstructionError::CallDepth)
    }

    /// Returns a view on the current instruction
    pub fn get_current_instruction_context(
        &self,
    ) -> Result<InstructionContext<'_, '_>, InstructionError> {
        let index_in_trace = self.get_current_instruction_index()?;
        self.get_instruction_context_at_index_in_trace(index_in_trace)
    }

    /// Returns a view on the next instruction. This function assumes it has already been
    /// configured with the correct values in `prepare_next_instruction` or
    /// `prepare_next_top_level_instruction`
    pub fn get_next_instruction_context(
        &self,
    ) -> Result<InstructionContext<'_, '_>, InstructionError> {
        let index_in_trace = if self.instruction_stack.is_empty() {
            self.next_top_level_instruction_index
        } else {
            self.instruction_trace
                .len()
                .checked_sub(1)
                .ok_or(InstructionError::CallDepth)?
        };
        self.get_instruction_context_at_index_in_trace(index_in_trace)
    }

    /// Configures an instruction at a specific index in trace.
    pub fn configure_instruction_at_index(
        &mut self,
        instruction_index: usize,
        program_index: IndexOfAccount,
        instruction_accounts: Vec<InstructionAccount>,
        deduplication_map: Vec<u16>,
        instruction_data: Cow<'ix_data, [u8]>,
        caller_index: Option<u16>,
    ) -> Result<(), InstructionError> {
        debug_assert_eq!(deduplication_map.len(), MAX_ACCOUNTS_PER_TRANSACTION);

        let instruction = self
            .instruction_trace
            .get_mut(instruction_index)
            .ok_or(InstructionError::MaxInstructionTraceLengthExceeded)?;

        // If we have a parent index, then we are dealing with a CPI.
        if let Some(caller_index) = caller_index {
            instruction.index_of_caller_instruction = caller_index;
            self.transaction_frame.configure_cpi();
        }

        instruction.program_account_index_in_tx = program_index;
        instruction.configure_vm_slices(
            instruction_index as u64,
            instruction_accounts.len(),
            instruction_data.len() as u64,
        );

        *self
            .deduplication_maps
            .get_mut(instruction_index)
            .ok_or(InstructionError::MaxInstructionTraceLengthExceeded)? =
            deduplication_map.into_boxed_slice();

        *self
            .instruction_accounts
            .get_mut(instruction_index)
            .ok_or(InstructionError::MaxInstructionTraceLengthExceeded)? = instruction_accounts;

        *self
            .instruction_data
            .get_mut(instruction_index)
            .ok_or(InstructionError::MaxInstructionTraceLengthExceeded)? = instruction_data;
        Ok(())
    }

    fn deduplicate_accounts(
        instruction_accounts: &mut [InstructionAccount],
    ) -> Result<Vec<u16>, InstructionError> {
        let mut dedup_map = vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION];
        for idx_in_ix in 0..instruction_accounts.len() {
            let first_occurrence_in_ix = dedup_map
                .get_mut(
                    instruction_accounts
                        .get(idx_in_ix)
                        .unwrap()
                        .index_in_transaction as usize,
                )
                .ok_or(InstructionError::MissingAccount)?;
            if *first_occurrence_in_ix == u16::MAX {
                *first_occurrence_in_ix = idx_in_ix as u16;
            } else {
                // Let's update the signer and writable flags for the first appearance of this
                // account.
                let [this_account, other_account] = instruction_accounts
                    .get_disjoint_mut([idx_in_ix, *first_occurrence_in_ix as usize])
                    .expect("Accounts indices must exist in array");

                other_account.set_is_signer(other_account.is_signer() || this_account.is_signer());
                other_account
                    .set_is_writable(other_account.is_writable() || this_account.is_writable());
            }
        }

        Self::replicate_account_flags(instruction_accounts, &dedup_map);
        Ok(dedup_map)
    }

    /// Replicate account flags to duplicated accounts
    pub fn replicate_account_flags(
        instruction_accounts: &mut [InstructionAccount],
        dedup_map: &[u16],
    ) {
        for current_index in 0..instruction_accounts.len() {
            let instruction_account = instruction_accounts.get(current_index).unwrap();
            let other_account_index = *dedup_map
                .get(instruction_account.index_in_transaction as usize)
                .expect("Deduplication map must contain this account")
                as usize;

            if current_index != other_account_index {
                let [current_account, reference_account] = instruction_accounts
                    .get_disjoint_mut([current_index, other_account_index])
                    .expect("Indices must be present in instruction account");

                current_account
                    .set_is_signer(current_account.is_signer() || reference_account.is_signer());
                current_account.set_is_writable(
                    current_account.is_writable() || reference_account.is_writable(),
                );
            }
        }
    }

    /// A version of `configure_top_level_instruction` to help creating the deduplication map in tests
    pub fn configure_top_level_instruction_for_tests(
        &mut self,
        program_index: IndexOfAccount,
        mut instruction_accounts: Vec<InstructionAccount>,
        instruction_data: Vec<u8>,
    ) -> Result<(), InstructionError> {
        debug_assert!(instruction_accounts.len() <= u16::MAX as usize);
        let dedup_map = Self::deduplicate_accounts(&mut instruction_accounts)?;

        self.configure_instruction_at_index(
            self.next_top_level_instruction_index,
            program_index,
            instruction_accounts,
            dedup_map,
            Cow::Owned(instruction_data),
            None,
        )?;
        Ok(())
    }

    /// A helper function to facilitate creating a CPI in tests
    pub fn configure_next_cpi_for_tests(
        &mut self,
        program_index: IndexOfAccount,
        mut instruction_accounts: Vec<InstructionAccount>,
        instruction_data: Vec<u8>,
    ) -> Result<(), InstructionError> {
        debug_assert!(instruction_accounts.len() <= u16::MAX as usize);
        let dedup_map = Self::deduplicate_accounts(&mut instruction_accounts)?;
        let caller_index = self.get_current_instruction_index()?;
        let cpi_index = self.get_instruction_trace_length();
        self.configure_instruction_at_index(
            cpi_index,
            program_index,
            instruction_accounts,
            dedup_map,
            Cow::Owned(instruction_data),
            Some(caller_index as u16),
        )?;
        Ok(())
    }

    /// Pushes the next instruction
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
            instruction.nesting_level = nesting_level as u16;
        }

        if self.number_of_called_instructions_in_trace() >= self.instruction_trace_capacity {
            return Err(InstructionError::MaxInstructionTraceLengthExceeded);
        }

        let (index_in_trace, current_top_level_instruction) = if self.instruction_stack.is_empty() {
            let index = self.next_top_level_instruction_index;
            self.next_top_level_instruction_index =
                self.next_top_level_instruction_index.saturating_add(1);
            (index, index)
        } else {
            let index = self.get_instruction_trace_length();
            self.transaction_frame.number_of_cpis_in_trace = self
                .transaction_frame
                .number_of_cpis_in_trace
                .saturating_add(1);
            self.instruction_trace.push(InstructionFrame::default());
            self.deduplication_maps.push(Box::default());
            self.instruction_accounts.push(Vec::new());
            self.instruction_data.push(Cow::Owned(Vec::new()));
            (
                index,
                self.next_top_level_instruction_index.saturating_sub(1),
            )
        };

        if nesting_level >= self.instruction_stack_capacity {
            return Err(InstructionError::CallDepth);
        }
        self.transaction_frame.current_executing_instruction = index_in_trace as u16;
        self.instruction_stack.push(index_in_trace);
        if let Some(index_in_transaction) = self.find_index_of_account(&instructions::id()) {
            let mut mut_account_ref = self.accounts.try_borrow_mut(index_in_transaction)?;
            if mut_account_ref.owner() != &solana_sdk_ids::sysvar::id() {
                return Err(InstructionError::InvalidAccountOwner);
            }
            instructions::store_current_index_checked(
                mut_account_ref.data_as_mut_slice(),
                current_top_level_instruction as u16,
            )?;
        }
        Ok(())
    }

    /// Pops the current instruction
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
        if let Some(instr_idx) = self.instruction_stack.last() {
            self.transaction_frame.current_executing_instruction = *instr_idx as u16;
        }
        if detected_an_unbalanced_instruction? {
            Err(InstructionError::UnbalancedInstruction)
        } else {
            Ok(())
        }
    }

    /// Gets the return data of the current instruction or any above
    pub fn get_return_data(&self) -> (&Pubkey, &[u8]) {
        (
            &self.transaction_frame.return_data_pubkey,
            &self.return_data_bytes,
        )
    }

    /// Set the return data of the current instruction
    pub fn set_return_data(
        &mut self,
        program_id: Pubkey,
        data: Vec<u8>,
    ) -> Result<(), InstructionError> {
        self.transaction_frame.return_data_pubkey = program_id;
        self.transaction_frame
            .return_data_scratchpad
            .set_len(data.len() as u64);
        self.return_data_bytes = data;
        Ok(())
    }

    pub fn abi_v2_access_violation_handler(&self) -> AccessViolationHandler {
        let accounts = Rc::clone(&self.accounts);
        Box::new(
            move |region: &mut MemoryRegion,
                  _address_space_reserved_for_account: u64,
                  access_type: AccessType,
                  _vm_addr: u64,
                  _len: u64| {
                if access_type == AccessType::Load {
                    return;
                }

                if region.host_buffer().is_mutable() {
                    // The region has already been made writable
                    return;
                }

                let Some(index_in_transaction) = region.access_violation_handler_payload else {
                    // This region is not a writable account.
                    return;
                };

                // The call below can't really fail. If they fail because of a bug,
                // whatever is writing will trigger an EbpfError::AccessViolation like
                // if the region was readonly, and the transaction will fail gracefully.
                let Ok(mut account) = accounts.try_borrow_mut(index_in_transaction) else {
                    debug_assert!(false);
                    return;
                };

                let Ok(()) = accounts.touch(index_in_transaction) else {
                    debug_assert!(false);
                    return;
                };

                // Only copy the account when the access is a store, otherwise no need to copy.
                *region = MemoryRegion::new(
                    &raw mut account.data_as_mut_slice()[..],
                    account.guest_pointer(),
                );
            },
        )
    }

    /// Returns a new account data write access handler for ABIv1
    pub fn access_violation_handler(
        &self,
        virtual_address_space_adjustments: bool,
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
                let region_vm_addr_start = region.vm_addr_range().start;
                let requested_length = vm_addr
                    .saturating_add(len)
                    .saturating_sub(region_vm_addr_start)
                    as usize;
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

                if requested_length > region.len() {
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
                    unsafe {
                        account.resize(new_len, 0);
                        // SAFETY:
                        //
                        // Contract from `MemoryRegion::redirect`: MemoryRegion must point to a
                        // valid object live for the duration of this `MemoryMapping`.
                        //
                        // Evidence: There are two distinct cases, when the account buffer is
                        // serialized and when the account buffer is directly mapped.
                        // * In the serialization case we continue pointing at the same buffer as
                        // before, and the original buffer must have satisfied the liveness
                        // condition before.
                        // * In the direct mapping case `account.resize` invalidates the buffer this
                        // region has been pointing at, but this is fixed up later in the "unshare"
                        // branch later.
                        //
                        // Contract from `MemoryRegion::redirect`: For `MemoryRegion`s marked
                        // writable, the host buffer must accept arbitrary bytes being overwritten
                        // without it resulting in unsoundness.
                        //
                        // Evidence: The account payloads dont have any internal soundness
                        // invariants. The buffer in the serialization case starts off and remains
                        // writable (even though the HostBuffer might have been initially created as
                        // immutable.) In the direct mapping case we redirect the region to the
                        // buffer stored in the account later on.
                        //
                        // Contract from `HostBuffer::mutable`: This host buffer must have been
                        // initially constructed with a mutable pointer.
                        // Evidence: See `create_memory_region_of_account`. Direct mapping case
                        // later reconstructs this buffer from scratch. See below.
                        region.redirect(region.host_buffer().mutable());
                    }
                }

                // Potentially unshare / make the account shared data unique (CoW logic).
                if virtual_address_space_adjustments && account_data_direct_mapping {
                    unsafe {
                        // SAFETY: refer to the comment above.
                        region.redirect(account.raw_mut_data_slice());
                    }
                }
            },
        )
    }

    /// Take ownership of the instruction trace
    pub fn take_instruction_trace(&mut self) -> InstructionTrace<'_> {
        // The last frame is a placeholder for the next instruction to be executed, so it
        // is empty.
        self.instruction_trace.pop();
        self.instruction_accounts.pop();
        self.instruction_data.pop();
        (
            std::mem::take(&mut self.instruction_trace),
            std::mem::take(&mut self.instruction_accounts),
            std::mem::take(&mut self.instruction_data),
        )
    }

    /// Called instruction are those that the program runtime has already called into. It
    /// encompasses instructions under execution (e.g. all nested CPIs are already called) and
    /// finished ones.
    ///
    /// Top level instructions that have not yet been executed aren't considered called.
    pub fn number_of_called_instructions_in_trace(&self) -> usize {
        self.next_top_level_instruction_index
            .saturating_add(self.transaction_frame.number_of_cpis_in_trace as usize)
    }

    /// Return next top level instruction to execute
    pub fn next_top_level_instruction_index(&self) -> usize {
        self.next_top_level_instruction_index
    }

    /// Return number of CPIs in instruction trace
    pub fn number_of_cpis_in_trace(&self) -> usize {
        self.transaction_frame.number_of_cpis_in_trace as usize
    }

    /// Return the pointer address of the transaction frame
    pub fn transaction_frame_address(&self) -> *const TransactionFrame {
        &raw const self.transaction_frame
    }

    pub fn instruction_trace_as_raw_slice(&self) -> *const [InstructionFrame] {
        &raw const self.instruction_trace[..]
    }

    pub fn return_data_region(&mut self) -> MemoryRegion {
        MemoryRegion::new(
            &raw mut self.return_data_bytes[..],
            self.transaction_frame.return_data_scratchpad.ptr(),
        )
    }

    pub fn instruction_payload_regions(&self) -> impl Iterator<Item = MemoryRegion> {
        let trace_range = 0..self.get_instruction_trace_length();

        let populated_frames = self
            .instruction_trace
            .get(trace_range.clone())
            .unwrap()
            .iter()
            .zip(self.instruction_data.get(trace_range).unwrap().iter())
            .map(|(ix_frame, ix_data)| {
                MemoryRegion::new(&raw const ix_data[..], ix_frame.instruction_data.ptr())
            });

        populated_frames
            .chain(self.fill_missing_instruction_regions(GUEST_INSTRUCTION_DATA_BASE_ADDRESS))
    }

    pub fn instruction_accounts_regions(&self) -> impl Iterator<Item = MemoryRegion> {
        let trace_range = 0..self.get_instruction_trace_length();

        let populated_frames = self
            .instruction_trace
            .get(trace_range.clone())
            .unwrap()
            .iter()
            .zip(self.instruction_accounts.get(trace_range).unwrap().iter())
            .map(|(ix_frame, accounts)| {
                let len = ix_frame.instruction_accounts.len();
                let host_slice = std::ptr::slice_from_raw_parts(accounts.as_ptr(), len as usize);
                MemoryRegion::new(host_slice, ix_frame.instruction_accounts.ptr())
            });

        populated_frames
            .chain(self.fill_missing_instruction_regions(GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS))
    }

    fn fill_missing_instruction_regions(
        &self,
        base_address: u64,
    ) -> impl Iterator<Item = MemoryRegion> {
        // Fill the address for the rest of the regions
        let num_ixs = self.get_instruction_trace_length();
        (num_ixs..MAX_INSTRUCTION_TRACE_LENGTH).map(move |idx| {
            let vm_addr = base_address.saturating_add(GUEST_REGION_SIZE.saturating_mul(idx as u64));
            MemoryRegion::new_empty(vm_addr)
        })
    }

    pub fn resize_region(
        &mut self,
        old_region: &MemoryRegion,
        new_len: u64,
    ) -> Result<MemoryRegion, InstructionError> {
        let vm_address = old_region.vm_addr_range().start;
        let new_region = match vm_address {
            RETURN_DATA_SCRATCHPAD => {
                self.return_data_bytes.resize(new_len as usize, 0);
                let insn_ctx = self.get_current_instruction_context()?;
                self.transaction_frame.return_data_pubkey = *insn_ctx.get_program_key()?;
                self.transaction_frame
                    .return_data_scratchpad
                    .set_len(new_len);
                MemoryRegion::new(&raw mut self.return_data_bytes[..], vm_address)
            }
            GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS..GUEST_ACCOUNT_PAYLOAD_END_ADDRESS => {
                let account_address = vm_address
                    .checked_sub(GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS)
                    .ok_or(InstructionError::InvalidArgument)?;
                let account_index = abiv2_region_index_from_vm_address(account_address);
                let index_in_transaction =
                    u16::try_from(account_index).map_err(|_| InstructionError::MissingAccount)?;
                let insn_ctx = self.get_current_instruction_context()?;
                let index_in_instruction =
                    insn_ctx.get_index_of_account_in_instruction(index_in_transaction)?;
                let mut account = insn_ctx.try_borrow_instruction_account(index_in_instruction)?;
                account.resize_payload_region(new_len as usize)?
            }
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS..GUEST_INSTRUCTION_DATA_END_ADDRESS => {
                let ix_address = vm_address
                    .checked_sub(GUEST_INSTRUCTION_DATA_BASE_ADDRESS)
                    .ok_or(InstructionError::InvalidArgument)?;
                let ix_idx = abiv2_region_index_from_vm_address(ix_address);
                if !self.is_upcoming_cpi_ix_index(ix_idx) {
                    // Only the last region is supposed to be resized, since it is going to be
                    // used for CPI
                    return Err(InstructionError::InvalidArgument);
                }

                let ix_data = self
                    .instruction_data
                    .get_mut(ix_idx)
                    .ok_or(InstructionError::InvalidArgument)?;

                debug_assert!(
                    matches!(ix_data, Cow::Owned(_)),
                    "writable regions implies ownership of ix data"
                );
                let data_vec = ix_data.to_mut();

                data_vec.resize(new_len as usize, 0);

                let ix_frame = self
                    .instruction_trace
                    .get_mut(ix_idx)
                    .ok_or(InstructionError::InvalidArgument)?;

                ix_frame.instruction_data = VmSlice::new(
                    GUEST_INSTRUCTION_DATA_BASE_ADDRESS
                        .saturating_add(GUEST_REGION_SIZE.saturating_mul(ix_idx as u64)),
                    new_len,
                );

                self.transaction_frame.cpi_data_scratchpad.set_len(new_len);

                MemoryRegion::new(&raw mut data_vec[..], vm_address)
            }
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS..GUEST_INSTRUCTION_ACCOUNT_END_ADDRESS => {
                let ix_address = vm_address
                    .checked_sub(GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS)
                    .ok_or(InstructionError::InvalidArgument)?;
                let ix_idx = abiv2_region_index_from_vm_address(ix_address);

                if !self.is_upcoming_cpi_ix_index(ix_idx) {
                    // Only the last region is supposed to be resized, since it is going to be
                    // used for CPI
                    return Err(InstructionError::InvalidArgument);
                }

                // This constant and the assertion serve to ensure we will never divide by zero
                // and appease clippy.
                // PS: The assertion is done during compile time.
                const SZ: u64 = size_of::<InstructionAccount>() as u64;
                const _: () = assert!(SZ > 0);
                let number_of_accounts = new_len.saturating_div(SZ);

                if !new_len.is_multiple_of(SZ) {
                    return Err(InstructionError::InvalidArgument);
                }

                let ix_accs = self
                    .instruction_accounts
                    .last_mut()
                    .ok_or(InstructionError::InvalidArgument)?;

                ix_accs.resize(
                    number_of_accounts as usize,
                    InstructionAccount::new(0, false, false),
                );

                let ix_frame = self
                    .instruction_trace
                    .get_mut(ix_idx)
                    .ok_or(InstructionError::InvalidArgument)?;

                ix_frame.instruction_accounts = VmSlice::new(
                    GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                        .saturating_add(GUEST_REGION_SIZE.saturating_mul(ix_idx as u64)),
                    number_of_accounts,
                );

                self.transaction_frame
                    .cpi_accounts_scratchpad
                    .set_len(number_of_accounts);

                MemoryRegion::new(&raw mut ix_accs[..], vm_address)
            }
            _ => {
                return Err(InstructionError::InvalidArgument);
            }
        };
        Ok(new_region)
    }

    /// Return the guest pointer for the InstructionFrame address for the instruction
    /// under execution
    pub fn ix_frame_guest_ptr(&self) -> Result<u64, InstructionError> {
        let current_instruction_idx = self.get_current_instruction_index()?;
        Ok(INSTRUCTION_TRACE_AREA.saturating_add(
            size_of::<InstructionFrame>().saturating_mul(current_instruction_idx) as u64,
        ))
    }

    /// Return the InstructionFrame for the instruction under execution
    pub fn current_ix_frame(&self) -> Result<&InstructionFrame, InstructionError> {
        let current_instruction_idx = self.get_current_instruction_index()?;
        Ok(self
            .instruction_trace
            .get(current_instruction_idx)
            .expect("The frame for this instruction must exist"))
    }

    /// Build an ABIv2 instruction frame for CPI
    /// It receives the index of the callee program account in the transaction
    pub fn build_abi_v2_frame(
        &mut self,
        program_idx_in_tx: IndexOfAccount,
    ) -> Result<(), InstructionError> {
        // This unused program id must stay here so we can confirm the give index points to
        // an existing transaction account.
        let _program_id = self.get_key_of_account_at_index(program_idx_in_tx)?;
        self.transaction_frame.configure_cpi();
        let caller_instruction = self.get_current_instruction_index()?;
        let next_frame = self
            .instruction_trace
            .last_mut()
            .ok_or(InstructionError::CallDepth)?;
        next_frame.program_account_index_in_tx = program_idx_in_tx;
        next_frame.index_of_caller_instruction = caller_instruction as u16;

        let ix_accounts = self
            .instruction_accounts
            .last_mut()
            .ok_or(InstructionError::CallDepth)?;
        // Deduplicate the instruction accounts the caller wrote in the CPI scratchpad
        let dedup_map = Self::deduplicate_accounts(ix_accounts)?;
        *self
            .deduplication_maps
            .last_mut()
            .ok_or(InstructionError::CallDepth)? = dedup_map.into_boxed_slice();
        Ok(())
    }
}

/// Return data at the end of a transaction
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "wincode", derive(wincode::SchemaRead, wincode::SchemaWrite))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionReturnData {
    pub program_id: Pubkey,
    pub data: Vec<u8>,
}

/// Everything that needs to be recorded from a TransactionContext after execution
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub struct ExecutionRecord {
    pub accounts: Vec<KeyedAccountSharedData>,
    pub return_data: TransactionReturnData,
    /// Parallel to `accounts`: whether each account was modified by the VM.
    pub touched_flags: Box<[bool]>,
    pub accounts_resize_delta: i64,
}

/// Used by the bank in the runtime to write back the processed accounts and recorded instructions
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl From<TransactionContext<'_>> for ExecutionRecord {
    fn from(context: TransactionContext) -> Self {
        let (accounts, touched_flags, resize_delta) = Rc::try_unwrap(context.accounts)
            .expect("transaction_context.accounts has unexpected outstanding refs")
            .take();

        // The flags only needed interior mutability while the VM was running.
        // Now that we own them, unwrap the per-element `Cell`s into a plain
        // `Box<[bool]>`. `Vec::from` reuses the box's allocation and the mapped
        // collect reuses that same buffer in place (`Cell<bool>` and `bool` have
        // identical layout), so no reallocation occurs.
        let touched_flags: Box<[bool]> = Vec::from(touched_flags)
            .into_iter()
            .map(|flag| flag.into_inner())
            .collect();

        let return_data = TransactionReturnData {
            program_id: context.transaction_frame.return_data_pubkey,
            data: context.return_data_bytes,
        };

        Self {
            accounts,
            return_data,
            touched_flags,
            accounts_resize_delta: Cell::into_inner(resize_delta),
        }
    }
}

#[cfg(all(test, not(target_arch = "sbf"), not(target_arch = "bpf")))]
mod tests {
    use {super::*, std::sync::Arc};

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
                /* number_of_top_level_instructions */ 1,
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
            1,
        );

        transaction_context
            .configure_top_level_instruction_for_tests(
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

    #[test]
    fn test_instruction_shared_items() {
        let transaction_accounts = vec![(Pubkey::new_unique(), AccountSharedData::default()); 10];
        let mut transaction_context =
            TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 3);

        let instruction_accounts_1 = vec![
            InstructionAccount::new(0, false, true),
            InstructionAccount::new(3, true, false),
        ];
        transaction_context
            .configure_top_level_instruction_for_tests(
                1,
                instruction_accounts_1.clone(),
                vec![1, 2, 3, 4],
            )
            .unwrap();
        transaction_context.push().unwrap();
        transaction_context.pop().unwrap();

        let instruction_accounts_2 = vec![
            InstructionAccount::new(0, false, true),
            InstructionAccount::new(3, true, false),
            InstructionAccount::new(5, false, false),
        ];
        transaction_context
            .configure_top_level_instruction_for_tests(
                1,
                instruction_accounts_2.clone(),
                vec![5, 6, 7, 8, 9],
            )
            .unwrap();
        transaction_context.push().unwrap();
        transaction_context.pop().unwrap();

        let instruction_accounts_3 = vec![
            InstructionAccount::new(0, false, true),
            InstructionAccount::new(3, true, false),
            InstructionAccount::new(5, false, false),
            InstructionAccount::new(3, false, false),
            InstructionAccount::new(10, false, false),
        ];
        transaction_context
            .configure_top_level_instruction_for_tests(
                1,
                instruction_accounts_3.clone(),
                vec![10, 11],
            )
            .unwrap();
        transaction_context.push().unwrap();

        let first_ix_context = transaction_context
            .get_instruction_context_at_index_in_trace(0)
            .unwrap();
        assert_eq!(
            instruction_accounts_1.as_slice(),
            first_ix_context.instruction_accounts
        );
        assert_eq!(
            *first_ix_context.instruction_data,
            **transaction_context.instruction_data.first().unwrap()
        );
        for (idx_in_ix, acc) in instruction_accounts_1.iter().enumerate() {
            assert_eq!(
                *first_ix_context
                    .dedup_map
                    .get(acc.index_in_transaction as usize)
                    .unwrap(),
                idx_in_ix as u16
            );
        }

        let second_ix_context = transaction_context
            .get_instruction_context_at_index_in_trace(1)
            .unwrap();
        assert_eq!(
            instruction_accounts_2.as_slice(),
            second_ix_context.instruction_accounts
        );
        assert_eq!(
            *second_ix_context.instruction_data,
            **transaction_context.instruction_data.get(1).unwrap()
        );
        for (idx_in_ix, acc) in instruction_accounts_2.iter().enumerate() {
            assert_eq!(
                *second_ix_context
                    .dedup_map
                    .get(acc.index_in_transaction as usize)
                    .unwrap(),
                idx_in_ix as u16
            );
        }

        let third_ix_context = transaction_context
            .get_instruction_context_at_index_in_trace(2)
            .unwrap();
        assert_eq!(
            vec![
                InstructionAccount::new(0, false, true),
                InstructionAccount::new(3, true, false),
                InstructionAccount::new(5, false, false),
                InstructionAccount::new(3, true, false),
                InstructionAccount::new(10, false, false),
            ]
            .as_slice(),
            third_ix_context.instruction_accounts
        );

        assert_eq!(
            *third_ix_context.instruction_data,
            **transaction_context.instruction_data.get(2).unwrap()
        );
        for (idx_in_ix, acc) in instruction_accounts_3.iter().enumerate() {
            if idx_in_ix == 3 {
                assert_eq!(
                    *third_ix_context
                        .dedup_map
                        .get(acc.index_in_transaction as usize)
                        .unwrap(),
                    1
                );
            } else {
                assert_eq!(
                    *third_ix_context
                        .dedup_map
                        .get(acc.index_in_transaction as usize)
                        .unwrap(),
                    idx_in_ix as u16
                );
            }
        }
    }

    #[test]
    fn test_number_of_instructions() {
        let transaction_accounts = vec![(Pubkey::new_unique(), AccountSharedData::default()); 3];
        let mut transaction_context =
            TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 2);
        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            0
        );

        // Instruction #0
        transaction_context
            .configure_instruction_at_index(
                0,
                0,
                vec![InstructionAccount::new(1, false, false)],
                vec![0; MAX_ACCOUNTS_PER_TRANSACTION],
                Vec::new().into(),
                None,
            )
            .unwrap();

        // Instruction #1
        transaction_context
            .configure_instruction_at_index(
                1,
                0,
                vec![InstructionAccount::new(1, false, false)],
                vec![0; MAX_ACCOUNTS_PER_TRANSACTION],
                Vec::new().into(),
                None,
            )
            .unwrap();

        // Executing instruction #0
        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            0
        );
        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            1
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            2
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            0
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(GUEST_REGION_SIZE.saturating_mul(2))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .len(),
            0,
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_accounts_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(2))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .len(),
            0,
        );

        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            1
        );

        // Instruction #0 does a CPI.
        transaction_context
            .configure_next_cpi_for_tests(
                0,
                vec![InstructionAccount::new(2, false, true)],
                Vec::new(),
            )
            .unwrap();

        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            2
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            3
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            1
        );
        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            2
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(GUEST_REGION_SIZE.saturating_mul(3))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_accounts_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(3))
        );

        // A nested CPI
        transaction_context
            .configure_next_cpi_for_tests(
                0,
                vec![InstructionAccount::new(2, false, true)],
                Vec::new(),
            )
            .unwrap();

        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            3
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            4
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(GUEST_REGION_SIZE.saturating_mul(4))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_accounts_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(4))
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            2
        );

        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            3
        );
        // Return from nested CPI
        transaction_context.pop().unwrap();
        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            3
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            4
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            2,
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            2
        );

        // A second nested CPI
        transaction_context
            .configure_next_cpi_for_tests(
                0,
                vec![InstructionAccount::new(2, false, true)],
                Vec::new(),
            )
            .unwrap();

        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            4
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            5
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(GUEST_REGION_SIZE.saturating_mul(5))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_accounts_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(5))
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            3
        );
        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            4
        );

        // Return from second nested CPI
        transaction_context.pop().unwrap();

        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            2
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            5
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(GUEST_REGION_SIZE.saturating_mul(5))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_accounts_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(5))
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            3
        );

        // Return from first CPI
        transaction_context.pop().unwrap();
        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            4
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            0
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            5
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(GUEST_REGION_SIZE.saturating_mul(5))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_accounts_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(5))
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            3,
        );

        // Let's go to Instruction #1 (top level)
        transaction_context.pop().unwrap();
        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            1,
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            3
        );

        // Instruction #1 will do a CPI.
        transaction_context
            .configure_next_cpi_for_tests(
                0,
                vec![InstructionAccount::new(2, false, true)],
                Vec::new(),
            )
            .unwrap();

        transaction_context.push().unwrap();

        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            5,
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .total_number_of_instructions_in_trace,
            6
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_data_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(GUEST_REGION_SIZE.saturating_mul(6))
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .cpi_accounts_scratchpad
                .ptr(),
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(6))
        );

        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            4
        );
        assert_eq!(
            transaction_context.number_of_called_instructions_in_trace(),
            6
        );

        // Return from CPI
        transaction_context.pop().unwrap();
        assert_eq!(
            transaction_context
                .transaction_frame
                .number_of_cpis_in_trace,
            4
        );
        assert_eq!(
            transaction_context
                .transaction_frame
                .current_executing_instruction,
            1,
        );

        transaction_context.pop().unwrap();
    }

    #[test]
    fn test_get_current_instruction_index() {
        let transaction_accounts = vec![(Pubkey::new_unique(), AccountSharedData::default()); 3];
        let mut transaction_context =
            TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 2);

        // First top level instruction
        transaction_context
            .configure_instruction_at_index(
                0,
                1,
                vec![
                    InstructionAccount::new(0, false, false),
                    InstructionAccount::new(1, false, false),
                ],
                vec![u16::MAX; 256],
                Cow::Owned(Vec::new()),
                None,
            )
            .unwrap();

        // Second top-level instruction
        transaction_context
            .configure_instruction_at_index(
                1,
                1,
                vec![
                    InstructionAccount::new(0, false, false),
                    InstructionAccount::new(1, false, true),
                ],
                vec![u16::MAX; 256],
                Cow::Owned(Vec::new()),
                None,
            )
            .unwrap();

        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context.get_current_instruction_index().unwrap(),
            0
        );

        transaction_context.pop().unwrap();

        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context.get_current_instruction_index().unwrap(),
            1
        );

        // Simulating a CPI
        transaction_context
            .configure_next_cpi_for_tests(
                1,
                vec![
                    InstructionAccount::new(0, false, true),
                    InstructionAccount::new(1, false, false),
                ],
                Vec::new(),
            )
            .unwrap();
        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context.get_current_instruction_index().unwrap(),
            2
        );

        // Yet another CPI
        transaction_context
            .configure_next_cpi_for_tests(
                1,
                vec![
                    InstructionAccount::new(0, false, true),
                    InstructionAccount::new(1, false, false),
                ],
                Vec::new(),
            )
            .unwrap();
        transaction_context.push().unwrap();
        assert_eq!(
            transaction_context.get_current_instruction_index().unwrap(),
            3
        );

        // CPI return
        transaction_context.pop().unwrap();
        assert_eq!(
            transaction_context.get_current_instruction_index().unwrap(),
            2
        );

        // CPI return 2
        transaction_context.pop().unwrap();
        assert_eq!(
            transaction_context.get_current_instruction_index().unwrap(),
            1
        );
    }

    #[test]
    fn test_instruction_payload_regions() {
        let instruction_trace = vec![
            InstructionFrame {
                reserved: 0,
                program_account_index_in_tx: 1,
                nesting_level: 0,
                index_of_caller_instruction: 3,
                instruction_accounts: VmSlice::new(0, 0),
                instruction_data: VmSlice::new(1, 5),
            },
            InstructionFrame {
                reserved: 0,
                program_account_index_in_tx: 2,
                nesting_level: 0,
                index_of_caller_instruction: 3,
                instruction_accounts: VmSlice::new(0, 0),
                instruction_data: VmSlice::new(2, 4),
            },
            InstructionFrame::default(), // Placeholder for the next instruction
        ];
        let p1 = vec![1u8, 2, 3, 4, 5];
        let p2 = vec![1u8, 2, 3, 4];

        let tx_ctx = TransactionContext {
            accounts: Rc::new(TransactionAccounts::new(Vec::new())),
            instruction_stack_capacity: 0,
            instruction_trace_capacity: 0,
            instruction_stack: vec![],
            instruction_trace,
            transaction_frame: TransactionFrame {
                return_data_pubkey: Pubkey::default(),
                return_data_scratchpad: VmSlice::new(0, 0),
                cpi_data_scratchpad: VmSlice::new(0, 0),
                cpi_accounts_scratchpad: VmSlice::new(0, 0),
                current_executing_instruction: 0,
                total_number_of_instructions_in_trace: 0,
                number_of_cpis_in_trace: 0,
                number_of_transaction_accounts: 0,
            },
            return_data_bytes: vec![],
            next_top_level_instruction_index: 0,
            rent: Rent::default(),
            deduplication_maps: vec![],
            instruction_accounts: vec![],
            instruction_data: vec![Cow::Borrowed(&p1), Cow::Borrowed(&p2)],
        };

        let regions = tx_ctx.instruction_payload_regions().collect::<Vec<_>>();

        let r1 = regions.first().unwrap();
        assert_eq!(r1.vm_addr_range().start, 1);
        assert_eq!(r1.len(), 5);
        assert_eq!(r1.host_buffer().ptr().cast(), p1.as_ptr());

        let r2 = regions.get(1).unwrap();
        assert_eq!(r2.vm_addr_range().start, 2);
        assert_eq!(r2.len(), 4);
        assert_eq!(r2.host_buffer().ptr().cast(), p2.as_ptr());

        let r3 = regions.get(2).unwrap();
        assert_eq!(
            r3.vm_addr_range().start,
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(2u64))
        );
        assert_eq!(r3.len(), 0);

        let r4 = regions.get(3).unwrap();
        assert_eq!(
            r4.vm_addr_range().start,
            GUEST_INSTRUCTION_DATA_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(3u64))
        );
        assert_eq!(r4.len(), 0);
    }

    #[test]
    fn test_instruction_account_regions() {
        let instruction_trace = vec![
            InstructionFrame {
                reserved: 0,
                program_account_index_in_tx: 1,
                nesting_level: 0,
                index_of_caller_instruction: 3,
                instruction_accounts: VmSlice::new(3, 4),
                instruction_data: VmSlice::new(0, 0),
            },
            InstructionFrame {
                reserved: 0,
                program_account_index_in_tx: 2,
                nesting_level: 0,
                index_of_caller_instruction: 3,
                instruction_accounts: VmSlice::new(1, 2),
                instruction_data: VmSlice::new(0, 0),
            },
            InstructionFrame::default(), // Placeholder for the next instruction
        ];
        let acc_1 = vec![InstructionAccount::default(); 4];
        let acc_2 = vec![InstructionAccount::default(); 2];

        let tx_ctx = TransactionContext {
            accounts: Rc::new(TransactionAccounts::new(Vec::new())),
            instruction_stack_capacity: 0,
            instruction_trace_capacity: 0,
            instruction_stack: vec![],
            instruction_trace,
            transaction_frame: TransactionFrame {
                return_data_pubkey: Pubkey::default(),
                return_data_scratchpad: VmSlice::new(0, 0),
                cpi_data_scratchpad: VmSlice::new(0, 0),
                cpi_accounts_scratchpad: VmSlice::new(0, 0),
                current_executing_instruction: 0,
                total_number_of_instructions_in_trace: 0,
                number_of_cpis_in_trace: 0,
                number_of_transaction_accounts: 0,
            },
            return_data_bytes: vec![],
            next_top_level_instruction_index: 0,
            rent: Rent::default(),
            deduplication_maps: vec![],
            instruction_accounts: vec![acc_1, acc_2],
            instruction_data: vec![],
        };

        let regions = tx_ctx.instruction_accounts_regions().collect::<Vec<_>>();

        let r1 = dbg!(&regions).first().unwrap();
        assert_eq!(r1.vm_addr_range().start, 3);
        assert_eq!(
            r1.len(),
            4usize.saturating_mul(size_of::<InstructionAccount>())
        );
        assert_eq!(
            r1.host_buffer().ptr().cast(),
            tx_ctx.instruction_accounts.first().unwrap().as_ptr()
        );

        let r2 = regions.get(1).unwrap();
        assert_eq!(r2.vm_addr_range().start, 1);
        assert_eq!(
            r2.len(),
            2usize.saturating_mul(size_of::<InstructionAccount>())
        );
        assert_eq!(
            r2.host_buffer().ptr().cast(),
            tx_ctx.instruction_accounts.get(1).unwrap().as_ptr()
        );

        let r3 = regions.get(2).unwrap();
        assert_eq!(
            r3.vm_addr_range().start,
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(2u64))
        );
        assert_eq!(r3.len(), 0);

        let r4 = regions.get(3).unwrap();
        assert_eq!(
            r4.vm_addr_range().start,
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS
                .saturating_add(GUEST_REGION_SIZE.saturating_mul(3u64))
        );
        assert_eq!(r4.len(), 0);
    }

    #[test]
    fn test_deduplicate_accounts() {
        let mut instruction_accounts = vec![
            InstructionAccount::new(0, false, true), // Account 0, writable
            InstructionAccount::new(1, true, false), // Account 1, signer
            InstructionAccount::new(0, false, false), // Account 0 again, not writable
            InstructionAccount::new(2, false, true), // Account 2, writable
            InstructionAccount::new(1, true, false), // Account 1 again, signer
        ];

        let dedup_map =
            TransactionContext::deduplicate_accounts(&mut instruction_accounts).unwrap();

        // Check that the dedup_map correctly maps duplicate accounts
        assert_eq!(
            *dedup_map.first().unwrap(),
            0,
            "account must be a duplicate of itself"
        );
        assert_eq!(
            *dedup_map.get(1).unwrap(),
            1,
            "account must be a duplicate of itself"
        );
        assert_eq!(
            *dedup_map.get(2).unwrap(),
            3,
            "account must be a duplicate of itself"
        );

        // Check that duplicate accounts are properly merged
        let acc = instruction_accounts.first().unwrap();
        assert_eq!(acc.index_in_transaction, 0);
        assert!(
            !acc.is_signer(),
            "Must not be a signer because account 1 is not signer"
        );
        assert!(
            acc.is_writable(),
            "Must be writable because account 0 is writable"
        );

        let acc = instruction_accounts.get(1).unwrap();
        assert_eq!(acc.index_in_transaction, 1);
        assert!(
            acc.is_signer(),
            "Must be signer because account 1 is signer"
        );
        assert!(
            !acc.is_writable(),
            "Must not be writable because account 1 is not writable"
        );

        let acc = instruction_accounts.get(2).unwrap();
        assert_eq!(acc.index_in_transaction, 0);
        assert!(!acc.is_signer(), "Should be merged from account 1");
        assert!(acc.is_writable(), "Should be merged from account 0");

        let acc = instruction_accounts.get(3).unwrap();
        assert_eq!(acc.index_in_transaction, 2);
        assert!(!acc.is_signer());
        assert!(acc.is_writable());

        let acc = instruction_accounts.get(4).unwrap();
        assert_eq!(acc.index_in_transaction, 1);
        assert!(
            acc.is_signer(),
            "Must be signer because account 1 is signer"
        );
        assert!(
            !acc.is_writable(),
            "Must not be writable because account 1 is not writable"
        );

        // Verify that the deduplication map correctly identifies duplicates
        assert_eq!(
            *dedup_map.first().unwrap(),
            0,
            "account must be a duplicate of itself"
        );
        assert_eq!(
            *dedup_map.get(1).unwrap(),
            1,
            "account must be a duplicate of itself"
        );
        assert_eq!(
            *dedup_map.get(2).unwrap(),
            3,
            "account must be a duplicate of itself"
        );
    }

    #[test]
    fn test_deduplicate_accounts_no_duplicates() {
        let mut instruction_accounts = vec![
            InstructionAccount::new(0, false, true),
            InstructionAccount::new(1, true, false),
            InstructionAccount::new(2, false, false),
        ];

        let dedup_map =
            TransactionContext::deduplicate_accounts(&mut instruction_accounts).unwrap();

        // Check that the dedup_map correctly maps each account to itself
        assert_eq!(
            *dedup_map.first().unwrap(),
            0,
            "account must be a duplicate of itself"
        );
        assert_eq!(
            *dedup_map.get(1).unwrap(),
            1,
            "account must be a duplicate of itself"
        );
        assert_eq!(
            *dedup_map.get(2).unwrap(),
            2,
            "account must be a duplicate of itself"
        );

        // Check that accounts are not modified
        let acc = instruction_accounts.first().unwrap();
        assert_eq!(acc.index_in_transaction, 0);
        assert!(!acc.is_signer());
        assert!(acc.is_writable());

        let acc = instruction_accounts.get(1).unwrap();
        assert_eq!(acc.index_in_transaction, 1);
        assert!(acc.is_signer());
        assert!(!acc.is_writable());

        let acc = instruction_accounts.get(2).unwrap();
        assert_eq!(acc.index_in_transaction, 2);
        assert!(!acc.is_signer());
        assert!(!acc.is_writable());
    }

    #[test]
    fn test_deduplicate_accounts_all_duplicates() {
        let mut instruction_accounts = vec![
            InstructionAccount::new(0, false, true),
            InstructionAccount::new(0, true, false),
            InstructionAccount::new(0, false, false),
        ];

        let dedup_map =
            TransactionContext::deduplicate_accounts(&mut instruction_accounts).unwrap();

        // Check that all accounts map to the first occurrence (index 0)
        assert_eq!(
            *dedup_map.first().unwrap(),
            0,
            "account must be a duplicate of itself"
        );
        for idx in dedup_map.iter().skip(1) {
            assert_eq!(*idx, u16::MAX);
        }

        // Check that the first account has combined flags
        let acc = instruction_accounts.first().unwrap();
        assert_eq!(acc.index_in_transaction, 0);
        assert!(
            acc.is_signer(),
            "Should be signer because of second account"
        );
        assert!(
            acc.is_writable(),
            "Should be writable because of first account"
        );

        // Check that the other accounts have the same flags as the first
        let acc = instruction_accounts.get(1).unwrap();
        assert_eq!(acc.index_in_transaction, 0);
        assert!(acc.is_signer());
        assert!(acc.is_writable());

        let acc = instruction_accounts.get(2).unwrap();
        assert_eq!(acc.index_in_transaction, 0);
        assert!(acc.is_signer());
        assert!(acc.is_writable());
    }

    #[test]
    fn test_abi_v2_access_violation_handler() {
        let accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::create_from_existing_shared_data(
                    40,
                    Arc::new(vec![1, 2]),
                    Pubkey::new_unique(),
                    false,
                    2,
                ),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::create_from_existing_shared_data(
                    20,
                    Arc::new(vec![3, 4]),
                    Pubkey::new_unique(),
                    false,
                    2,
                ),
            ),
        ];

        let tx_context = TransactionContext::new(accounts, Rent::default(), 4, 4, 2);

        let data = [0u8; 8];
        let mut region = MemoryRegion::new(&raw const data[..], 0x80);
        let handler = tx_context.abi_v2_access_violation_handler();

        handler(&mut region, 0, AccessType::Load, 0, 0);
        assert!(!region.host_buffer().is_mutable());
        assert_eq!(region.host_buffer().ptr().cast(), data.as_ptr());

        unsafe {
            region.redirect(region.host_buffer().mutable());
        }
        handler(&mut region, 0, AccessType::Store, 0, 0);
        assert!(region.host_buffer().is_mutable());
        assert_eq!(region.host_buffer().ptr().cast(), data.as_ptr());

        region.make_immutable();
        handler(&mut region, 0, AccessType::Store, 0, 0);
        assert!(!region.host_buffer().is_mutable());
        assert_eq!(region.host_buffer().ptr().cast(), data.as_ptr());

        region.access_violation_handler_payload = Some(1);
        region.make_immutable();
        handler(&mut region, 0, AccessType::Store, 0, 0);
        assert!(region.host_buffer().is_mutable());
        assert!(region.access_violation_handler_payload.is_none());
        assert_eq!(
            region.host_buffer().ptr().cast(),
            tx_context.accounts.try_borrow(1).unwrap().data().as_ptr()
        );
    }

    #[test]
    fn test_set_abi_v2_frame_wrong_input() {
        let transaction_accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::new(20, 8, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(30, 8, &Pubkey::new_unique()),
            ),
        ];
        let mut tx_context =
            TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 1);

        tx_context
            .configure_instruction_at_index(
                0,
                0,
                vec![InstructionAccount::new(1, false, false)],
                vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION],
                Cow::Owned(Vec::new()),
                None,
            )
            .unwrap();

        tx_context.push().unwrap();

        // Let's configure the CPI
        {
            let ix_accounts = tx_context.instruction_accounts.last_mut().unwrap();
            ix_accounts.push(InstructionAccount::new(0, false, false));
        }

        // Non-existing program id
        // A program account index that is not part of the transaction
        let result = tx_context.build_abi_v2_frame(9);
        assert_eq!(result.err().unwrap(), InstructionError::MissingAccount);

        // Push a nonexisting account in the array
        // This part actually checks if the function `deduplicate_accounts` will return an error
        // for a nonexisting account.
        {
            let ix_accounts = tx_context.instruction_accounts.last_mut().unwrap();
            ix_accounts.push(InstructionAccount::new(300, false, false));
        }

        let result = tx_context.build_abi_v2_frame(1);
        assert_eq!(result.err().unwrap(), InstructionError::MissingAccount);
    }
}
