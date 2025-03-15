use {solana_pubkey::Pubkey, solana_svm_transaction::instruction::SVMInstruction};

#[derive(Default)]
pub struct InstructionDataLenBuilder {
    value: u64,
}

impl InstructionDataLenBuilder {
    pub fn process_instruction(&mut self, _program_id: &Pubkey, instruction: &SVMInstruction) {
        self.value = self.value.saturating_add(instruction.data.len() as u64);
    }

    pub fn build(self) -> u64 {
        self.value
    }
}
