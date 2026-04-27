use {
    crate::invoke_context::BpfAllocator,
    solana_instruction::error::InstructionError,
    solana_sbpf::{
        ebpf::MM_BYTECODE_START,
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
            INSTRUCTION_TRACE_AREA, RETURN_DATA_SCRATCHPAD, TRANSACTION_FRAME_ADDRESS,
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

    // Filled on a later stage
    // Index 0: ELF rodata
    // Index 1: ELF text area (not mapped)
    static EMPTY: [u8; 0] = [];
    *v2_regions
        .get_mut((MM_BYTECODE_START >> 32) as usize)
        .unwrap() = MemoryRegion::new(&raw const EMPTY, MM_BYTECODE_START);

    // Index 2: heap
    // Index 3: stack

    // Index 4: Transaction frame area
    *v2_regions
        .get_mut((TRANSACTION_FRAME_ADDRESS >> 32) as usize)
        .unwrap() = MemoryRegion::new(
        transaction_context.transaction_frame_address(),
        TRANSACTION_FRAME_ADDRESS,
    );

    // Index 5: Accounts metadata area
    let accounts_slice = transaction_context.accounts().shared_fields_as_raw_slice();
    *v2_regions
        .get_mut((ACCOUNT_METADATA_AREA >> 32) as usize)
        .unwrap() = MemoryRegion::new(accounts_slice, ACCOUNT_METADATA_AREA);

    // Index 6: Instruction metadata area
    let instruction_trace_slice = transaction_context.instruction_trace_as_raw_slice();
    *v2_regions
        .get_mut((INSTRUCTION_TRACE_AREA >> 32) as usize)
        .unwrap() = MemoryRegion::new(instruction_trace_slice, INSTRUCTION_TRACE_AREA);

    // Index 7: Return data scratchpad area
    let return_data_slice = transaction_context.return_data_as_raw_slice();
    *v2_regions
        .get_mut((RETURN_DATA_SCRATCHPAD >> 32) as usize)
        .unwrap() = MemoryRegion::new(return_data_slice, RETURN_DATA_SCRATCHPAD);

    // Indexes 8..264: Transaction accounts payload
    {
        let accounts_index = (GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS >> 32) as usize;
        let range = accounts_index..accounts_index.saturating_add(MAX_ACCOUNTS_PER_TRANSACTION);
        let account_regions = v2_regions.get_mut(range).unwrap();
        transaction_context
            .accounts()
            .account_payload_regions(account_regions);
    }

    // Indexes 264..328: Instruction data payload area
    {
        let ix_payload_index = (GUEST_INSTRUCTION_DATA_BASE_ADDRESS >> 32) as usize;
        let range = ix_payload_index..ix_payload_index.saturating_add(MAX_INSTRUCTION_TRACE_LENGTH);
        let regions = v2_regions.get_mut(range).unwrap();
        transaction_context.instruction_payload_regions(regions);
    }

    // Indexes 328..392: Instruction accounts area
    {
        let ix_accounts_index = (GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS >> 32) as usize;
        let range =
            ix_accounts_index..ix_accounts_index.saturating_add(MAX_INSTRUCTION_TRACE_LENGTH);
        let regions = v2_regions.get_mut(range).unwrap();
        transaction_context.instruction_accounts_regions(regions);
    }

    v2_regions
}
