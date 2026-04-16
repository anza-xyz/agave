use {
    crate::invoke_context::BpfAllocator,
    solana_instruction::error::InstructionError,
    solana_sbpf::{memory_region::MemoryMapping, program::SBPFVersion, vm::Config},
};

pub struct MemoryContexts(pub Vec<MemoryContext>);

impl MemoryContexts {
    /// Set this instruction's [`MemoryContext`].
    pub fn set_memory_context(
        &mut self,
        memory_context: MemoryContext,
    ) -> Result<(), InstructionError> {
        *self.0.last_mut().ok_or(InstructionError::CallDepth)? = memory_context;
        Ok(())
    }

    /// Get current instruction's [`MemoryContext`]
    pub fn memory_context(&self) -> Result<&MemoryContext, InstructionError> {
        self.0.last().ok_or(InstructionError::CallDepth)
    }

    /// Get current instruction's [`MemoryContext`] for mutable use.
    pub fn memory_context_mut(&mut self) -> Result<&mut MemoryContext, InstructionError> {
        self.0.last_mut().ok_or(InstructionError::CallDepth)
    }

    pub fn memory_mapping(&self) -> Result<&MemoryMapping, InstructionError> {
        let last_context = self.memory_context()?;
        Ok(&last_context.memory_mapping)
    }

    pub fn memory_mapping_mut(&mut self) -> Result<&mut MemoryMapping, InstructionError> {
        let last_context = self.memory_context_mut()?;
        Ok(&mut last_context.memory_mapping)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn mock_set_mapping(&mut self, memory_mapping: MemoryMapping) {
        self.0 = vec![MemoryContext {
            allocator: BpfAllocator::new(0),
            accounts_metadata: vec![],
            memory_mapping: Box::new(memory_mapping),
        }];
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

    /// Returns an empty dummy context used for builtin functions
    pub(crate) fn empty() -> Self {
        Self {
            allocator: BpfAllocator::new(0),
            accounts_metadata: Vec::new(),
            memory_mapping: Box::new(
                MemoryMapping::new(Vec::new(), &Config::default(), SBPFVersion::Reserved).unwrap(),
            ),
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
