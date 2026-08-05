//! Program-input parameter serialization, shared by the conformance harnesses.

use {
    solana_instruction::error::InstructionError,
    solana_message::SanitizedMessage,
    solana_program_runtime::{
        invoke_context::InvokeContext,
        memory_context::SerializedAccountMetadata,
        serialization::serialize_parameters,
        solana_sbpf::{
            aligned_memory::AlignedMemory, ebpf::HOST_ALIGN, memory_region::MemoryRegion,
        },
    },
    solana_svm_feature_set::SVMFeatureSet,
};

/// The product of serializing a program's input parameters into VM memory: the
/// serialized region itself plus the metadata needed to map accounts back out.
pub struct SerializedParameters {
    pub aligned_memory: AlignedMemory<HOST_ALIGN>,
    pub input_memory_regions: Vec<MemoryRegion>,
    pub account_metadata: Vec<SerializedAccountMetadata>,
}

/// Push the message's single top-level instruction onto `invoke_context`, then
/// serialize that instruction's program input parameters into VM memory.
pub fn push_and_serialize_parameters<'ix_data>(
    invoke_context: &mut InvokeContext<'_, 'ix_data>,
    sanitized_message: &'ix_data SanitizedMessage,
    feature_set: &SVMFeatureSet,
) -> Result<SerializedParameters, InstructionError> {
    invoke_context
        .prepare_top_level_instructions(sanitized_message)
        .expect("failed to prepare top-level instructions");
    invoke_context
        .push()
        .expect("failed to push instruction context");

    let instruction_context = invoke_context
        .transaction_context
        .get_current_instruction_context()
        .unwrap();
    serialize_parameters(
        &instruction_context,
        feature_set.virtual_address_space_adjustments,
        feature_set.account_data_direct_mapping,
        feature_set.direct_account_pointers_in_program_input,
    )
    .map(
        |(aligned_memory, input_memory_regions, account_metadata, _instruction_data_offset)| {
            SerializedParameters {
                aligned_memory,
                input_memory_regions,
                account_metadata,
            }
        },
    )
}
