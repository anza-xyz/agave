use {
    crate::invoke_context::BpfAllocator,
    solana_instruction::error::InstructionError,
    solana_sbpf::{
        ebpf::{MM_BYTECODE_START, MM_RODATA_START},
        elf::Executable,
        memory_region::{MemoryMapping, MemoryRegion, default_access_violation_handler},
        program::SBPFVersion,
        vm::{Config, ContextObject},
    },
    solana_transaction_context::{
        MAX_ACCOUNTS_PER_TRANSACTION, MAX_INSTRUCTION_TRACE_LENGTH,
        transaction::TransactionContext,
        vm_addresses::{
            ACCOUNT_METADATA_AREA, GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS,
            GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS, GUEST_INSTRUCTION_DATA_BASE_ADDRESS,
            HEAP_ADDRESS, INSTRUCTION_TRACE_AREA, RETURN_DATA_SCRATCHPAD, STACK_ADDRESS,
            TRANSACTION_FRAME_ADDRESS, abiv2_region_index_from_vm_address,
        },
    },
};

const NUMBER_OF_REGIONS: usize = 392;

enum MemoryContextType {
    ABIv1(MemoryContext),
    Placeholder,
    ABIv2,
}

pub struct MemoryContexts {
    contexts: Vec<MemoryContextType>,
    abiv2_mappings: Box<MemoryMapping>,
}

impl MemoryContexts {
    pub(crate) fn new() -> Self {
        Self {
            contexts: Vec::new(),
            abiv2_mappings: Box::new(unsafe {
                MemoryMapping::new(Vec::new(), &Config::default(), SBPFVersion::Reserved).unwrap()
            }),
        }
    }

    /// Set this instruction's [`MemoryContext`].
    pub fn set_memory_context_abi_v1(
        &mut self,
        memory_context: MemoryContext,
    ) -> Result<(), InstructionError> {
        *self
            .contexts
            .last_mut()
            .ok_or(InstructionError::CallDepth)? = MemoryContextType::ABIv1(memory_context);
        Ok(())
    }

    /// Get current instruction's [`MemoryContext`]
    pub fn memory_context_abi_v1(&self) -> Result<&MemoryContext, InstructionError> {
        match self.contexts.last().ok_or(InstructionError::CallDepth)? {
            MemoryContextType::ABIv1(ctx) => Ok(ctx),
            MemoryContextType::Placeholder => Err(InstructionError::ProgramEnvironmentSetupFailure),
            MemoryContextType::ABIv2 => Err(InstructionError::InvalidAccountData),
        }
    }

    /// Get current instruction's [`MemoryContext`] for mutable use.
    pub fn memory_context_mut_abi_v1(&mut self) -> Result<&mut MemoryContext, InstructionError> {
        let context = self
            .contexts
            .last_mut()
            .ok_or(InstructionError::CallDepth)?;

        match context {
            MemoryContextType::ABIv1(ctx) => Ok(ctx),
            MemoryContextType::Placeholder => Err(InstructionError::ProgramEnvironmentSetupFailure),
            MemoryContextType::ABIv2 => Err(InstructionError::ProgramEnvironmentSetupFailure),
        }
    }

    pub fn memory_mapping(&self) -> Result<&MemoryMapping, InstructionError> {
        let mapping = match self.contexts.last().ok_or(InstructionError::CallDepth)? {
            MemoryContextType::ABIv1(ctx) => &ctx.memory_mapping,
            MemoryContextType::Placeholder => {
                return Err(InstructionError::ProgramEnvironmentSetupFailure);
            }
            MemoryContextType::ABIv2 => &self.abiv2_mappings,
        };

        Ok(mapping)
    }

    pub fn memory_mapping_mut(&mut self) -> Result<&mut MemoryMapping, InstructionError> {
        let mapping = match self
            .contexts
            .last_mut()
            .ok_or(InstructionError::CallDepth)?
        {
            MemoryContextType::ABIv1(ctx) => &mut ctx.memory_mapping,
            MemoryContextType::Placeholder => {
                return Err(InstructionError::ProgramEnvironmentSetupFailure);
            }
            MemoryContextType::ABIv2 => &mut self.abiv2_mappings,
        };

        Ok(mapping)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn mock_set_mapping_abi_v1(&mut self, memory_mapping: MemoryMapping) {
        self.contexts = vec![MemoryContextType::ABIv1(MemoryContext {
            allocator: BpfAllocator::new(0),
            accounts_metadata: vec![],
            memory_mapping: Box::new(memory_mapping),
        })];
    }

    pub fn push_placeholder(&mut self) {
        // We are only pushing a placeholder to be configured later
        self.contexts.push(MemoryContextType::Placeholder);
    }

    pub fn pop(&mut self) {
        self.contexts.pop();
    }

    pub fn abi_v2_regions_exist(&self) -> bool {
        !self.abiv2_mappings.get_regions().is_empty()
    }

    pub fn create_abi_v2_mappings<C: ContextObject>(
        &mut self,
        regions: Vec<MemoryRegion>,
        executable: &Executable<C>,
    ) {
        *self.abiv2_mappings = unsafe {
            MemoryMapping::new_uninitialized(
                regions,
                executable.get_config(),
                executable.get_sbpf_version(),
                Box::new(default_access_violation_handler),
            )
        };
    }

    pub fn set_abi_v2(&mut self) -> Result<(), InstructionError> {
        *self
            .contexts
            .last_mut()
            .ok_or(InstructionError::CallDepth)? = MemoryContextType::ABIv2;
        Ok(())
    }

    pub fn update_abi_v2_account_permissions(
        &mut self,
        transaction_context: &TransactionContext,
    ) -> Result<(), InstructionError> {
        let current_instruction = transaction_context.get_current_instruction_context()?;

        let accounts_index = abiv2_region_index_from_vm_address(GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS);
        let range = accounts_index..accounts_index.saturating_add(MAX_ACCOUNTS_PER_TRANSACTION);
        let account_regions = self
            .abiv2_mappings
            .get_regions_mut()
            .get_mut(range)
            .expect("Account regions should have been configured.");

        for account in current_instruction.instruction_accounts() {
            let acc_region = account_regions
                .get_mut(account.index_in_transaction as usize)
                .expect("Account must exist");
            acc_region.writable = account.is_writable();
        }

        Ok(())
    }
}

/// This structure contains metadata about the memory for each instruction under execution.
/// The BpfAllocator, accounts addresses in the guest and the memory mapping.
pub struct MemoryContext {
    pub allocator: BpfAllocator,
    pub accounts_metadata: Vec<SerializedAccountMetadata>,
    memory_mapping: Box<MemoryMapping>,
}

impl MemoryContext {
    /// Creates a new memory context
    pub fn new(
        allocator: BpfAllocator,
        accounts_metadata: Vec<SerializedAccountMetadata>,
        memory_mapping: MemoryMapping,
    ) -> Self {
        Self {
            allocator,
            accounts_metadata,
            memory_mapping: Box::new(memory_mapping),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SerializedAccountMetadata {
    /// Address of the first byte of the serialized account record (the
    /// `NON_DUP_MARKER`/duplicate-marker byte).
    pub vm_addr: u64,
    pub original_data_len: usize,
    pub vm_data_addr: u64,
    pub vm_key_addr: u64,
    pub vm_lamports_addr: u64,
    pub vm_owner_addr: u64,
}

pub(crate) fn create_abiv2_regions(transaction_context: &TransactionContext) -> Vec<MemoryRegion> {
    let mut v2_regions: Vec<MemoryRegion> = vec![MemoryRegion::default(); NUMBER_OF_REGIONS];

    // Filled on a later stage, but we still want to have at least base vm_addrs be accurate so that
    // there are no duplicate regions (for e.g. tests.)
    // Index 0: ELF rodata
    // Index 1: ELF text area (not mapped)
    // Index 2: heap
    // Index 3: stack
    for vm_addr in [
        MM_RODATA_START,
        MM_BYTECODE_START,
        HEAP_ADDRESS,
        STACK_ADDRESS,
    ] {
        v2_regions
            .get_mut(abiv2_region_index_from_vm_address(vm_addr))
            .unwrap()
            .vm_addr = vm_addr;
    }

    // Index 4: Transaction frame area
    let transaction_frame_region = MemoryRegion::new(
        transaction_context.transaction_frame_address(),
        TRANSACTION_FRAME_ADDRESS,
    );
    *v2_regions
        .get_mut(abiv2_region_index_from_vm_address(
            TRANSACTION_FRAME_ADDRESS,
        ))
        .unwrap() = transaction_frame_region;

    // Index 5: Accounts metadata area
    let accounts_slice = transaction_context.accounts().shared_fields_as_raw_slice();
    *v2_regions
        .get_mut(abiv2_region_index_from_vm_address(ACCOUNT_METADATA_AREA))
        .unwrap() = MemoryRegion::new(accounts_slice, ACCOUNT_METADATA_AREA);

    // Index 6: Instruction metadata area
    let instruction_trace_slice = transaction_context.instruction_trace_as_raw_slice();
    *v2_regions
        .get_mut(abiv2_region_index_from_vm_address(INSTRUCTION_TRACE_AREA))
        .unwrap() = MemoryRegion::new(instruction_trace_slice, INSTRUCTION_TRACE_AREA);

    // Index 7: Return data scratchpad area
    let return_data_slice = transaction_context.return_data_as_raw_slice();
    *v2_regions
        .get_mut(abiv2_region_index_from_vm_address(RETURN_DATA_SCRATCHPAD))
        .unwrap() = MemoryRegion::new(return_data_slice, RETURN_DATA_SCRATCHPAD);

    // Indexes 8..264: Transaction accounts payload
    let payload_start_idx = abiv2_region_index_from_vm_address(GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS);
    let account_regions = v2_regions
        .get_mut(payload_start_idx..payload_start_idx.saturating_add(MAX_ACCOUNTS_PER_TRANSACTION))
        .unwrap();
    transaction_context
        .accounts()
        .account_payload_regions(account_regions);

    // Indexes 264..328: Instruction data payload area
    let data_start_idx = abiv2_region_index_from_vm_address(GUEST_INSTRUCTION_DATA_BASE_ADDRESS);
    let regions = v2_regions
        .get_mut(data_start_idx..data_start_idx.saturating_add(MAX_INSTRUCTION_TRACE_LENGTH))
        .unwrap();
    transaction_context.instruction_payload_regions(regions);

    // Indexes 328..392: Instruction accounts area
    let acc_start_idx = abiv2_region_index_from_vm_address(GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS);
    let regions = v2_regions
        .get_mut(acc_start_idx..acc_start_idx.saturating_add(MAX_INSTRUCTION_TRACE_LENGTH))
        .unwrap();
    transaction_context.instruction_accounts_regions(regions);

    v2_regions
}

#[cfg(test)]
mod test {
    use {
        crate::memory_context::{MemoryContexts, create_abiv2_regions},
        solana_account::AccountSharedData,
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sbpf::{
            memory_region::{MemoryMapping, default_access_violation_handler},
            program::SBPFVersion,
            vm::Config,
        },
        solana_transaction_context::{
            MAX_ACCOUNTS_PER_TRANSACTION, instruction_accounts::InstructionAccount,
            transaction::TransactionContext, vm_addresses::GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS,
        },
        std::borrow::Cow,
    };

    #[test]
    fn test_update_account_permissions() {
        let accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::new(20, 10, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(30, 15, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(40, 5, &Pubkey::new_unique()),
            ),
        ];

        let mut tx_context = TransactionContext::new(accounts, Rent::default(), 4, 64, 3);

        tx_context
            .configure_instruction_at_index(
                0,
                0,
                vec![
                    InstructionAccount::new(0, false, false),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(1, false, false),
                ],
                vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION],
                Cow::Owned(Vec::new()),
                None,
            )
            .unwrap();

        tx_context
            .configure_instruction_at_index(
                0,
                0,
                vec![
                    InstructionAccount::new(1, false, false),
                    InstructionAccount::new(2, false, false),
                    InstructionAccount::new(0, false, true),
                ],
                vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION],
                Cow::Owned(Vec::new()),
                None,
            )
            .unwrap();

        tx_context
            .configure_instruction_at_index(
                0,
                0,
                vec![
                    InstructionAccount::new(0, false, true),
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, false),
                ],
                vec![u16::MAX; MAX_ACCOUNTS_PER_TRANSACTION],
                Cow::Owned(Vec::new()),
                None,
            )
            .unwrap();

        let mut memory_contexts = MemoryContexts::new();
        let abi_v2_regions = create_abiv2_regions(&tx_context);
        *memory_contexts.abiv2_mappings = unsafe {
            MemoryMapping::new_uninitialized(
                abi_v2_regions,
                &Config::default(),
                SBPFVersion::V4,
                Box::new(default_access_violation_handler),
            )
        };

        let accounts_range = ((GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS >> 32) as usize)
            ..((GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS >> 32) as usize)
                .saturating_add(MAX_ACCOUNTS_PER_TRANSACTION);

        // IX 1
        tx_context.push().unwrap();
        memory_contexts
            .update_abi_v2_account_permissions(&tx_context)
            .unwrap();
        let ix1_regions = memory_contexts
            .abiv2_mappings
            .get_regions()
            .get(accounts_range.clone())
            .unwrap();
        assert!(!ix1_regions.first().unwrap().writable);
        assert!(!ix1_regions.get(1).unwrap().writable);
        assert!(ix1_regions.get(2).unwrap().writable);
        for account_region in ix1_regions.iter().skip(3) {
            assert!(!account_region.writable);
        }

        // IX 2
        tx_context.pop().unwrap();
        tx_context.push().unwrap();
        memory_contexts
            .update_abi_v2_account_permissions(&tx_context)
            .unwrap();
        let ix2_regions = memory_contexts
            .abiv2_mappings
            .get_regions()
            .get(accounts_range.clone())
            .unwrap();
        assert!(ix2_regions.first().unwrap().writable);
        assert!(!ix2_regions.get(1).unwrap().writable);
        assert!(!ix2_regions.get(2).unwrap().writable);
        for account_region in ix2_regions.iter().skip(3) {
            assert!(!account_region.writable);
        }

        // IX 3
        tx_context.pop().unwrap();
        tx_context.push().unwrap();
        memory_contexts
            .update_abi_v2_account_permissions(&tx_context)
            .unwrap();
        let ix3_regions = memory_contexts
            .abiv2_mappings
            .get_regions()
            .get(accounts_range.clone())
            .unwrap();
        assert!(ix3_regions.first().unwrap().writable);
        assert!(ix3_regions.get(1).unwrap().writable);
        assert!(!ix3_regions.get(2).unwrap().writable);
        for account_region in ix3_regions.iter().skip(3) {
            assert!(!account_region.writable);
        }
    }
}
