#![cfg_attr(docsrs, feature(doc_auto_cfg))]
use solana_message::compiled_instruction::CompiledInstruction;

#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize),
    serde(rename_all = "camelCase")
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InnerInstruction {
    pub instruction: CompiledInstruction,
    /// Invocation stack height of this instruction. Instruction stack height
    /// starts at 1 for transaction instructions.
    pub stack_height: u8,
}

/// An ordered list of compiled instructions that were invoked during a
/// transaction instruction
pub type InnerInstructions = Vec<InnerInstruction>;

/// A list of compiled instructions that were invoked during each instruction of
/// a transaction
pub type InnerInstructionsList = Vec<InnerInstructions>;
