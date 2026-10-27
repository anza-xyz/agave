use {
    crate::compute_budget::ComputeUnitConfig,
    solana_clap_utils::compute_budget::ComputeUnitLimit,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_message::{Message, VersionedMessage},
    solana_program_runtime::execution_budget::MAX_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey,
    solana_system_interface::instruction::advance_nonce_account,
    spl_memo_interface::v4::id as memo_program_id,
};

/// Collects the instructions and transaction settings before compiling a CLI message.
pub(crate) struct MessageBuilder {
    instructions: Vec<Instruction>,
    compute_unit_price: Option<u64>,
    compute_unit_limit: ComputeUnitLimit,
    compute_unit_config_position: Option<usize>,
    nonce: Option<(Pubkey, Pubkey)>,
    blockhash: Option<Hash>,
}

impl MessageBuilder {
    pub(crate) fn new(instructions: Vec<Instruction>) -> Self {
        Self {
            instructions,
            compute_unit_price: None,
            compute_unit_limit: ComputeUnitLimit::Default,
            compute_unit_config_position: None,
            nonce: None,
            blockhash: None,
        }
    }

    pub(crate) fn with_memo<T: AsRef<str>>(mut self, memo: Option<T>) -> Self {
        if let Some(memo) = memo {
            self.instructions.push(Instruction {
                program_id: Pubkey::from(memo_program_id().to_bytes()),
                accounts: vec![],
                data: memo.as_ref().as_bytes().to_vec(),
            });
        }
        self
    }

    pub(crate) fn with_compute_unit_config(mut self, config: &ComputeUnitConfig) -> Self {
        self.compute_unit_price = config.compute_unit_price;
        self.compute_unit_limit = config.compute_unit_limit;
        self.compute_unit_config_position = Some(self.instructions.len());
        self
    }

    pub(crate) fn with_instruction(mut self, instruction: Instruction) -> Self {
        self.instructions.push(instruction);
        self
    }

    pub(crate) fn with_nonce(mut self, account: &Pubkey, authority: &Pubkey) -> Self {
        self.nonce = Some((*account, *authority));
        self
    }

    pub(crate) fn with_blockhash(mut self, blockhash: Hash) -> Self {
        self.blockhash = Some(blockhash);
        self
    }

    pub(crate) fn build(mut self, payer: Option<&Pubkey>) -> VersionedMessage {
        if let Some(price) = self.compute_unit_price {
            let mut budget_instructions =
                vec![ComputeBudgetInstruction::set_compute_unit_price(price)];
            match self.compute_unit_limit {
                ComputeUnitLimit::Default => {}
                ComputeUnitLimit::Static(limit) => budget_instructions
                    .push(ComputeBudgetInstruction::set_compute_unit_limit(limit)),
                ComputeUnitLimit::Simulated | ComputeUnitLimit::SimulatedWithExtraPercentage(_) => {
                    budget_instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                        MAX_COMPUTE_UNIT_LIMIT,
                    ));
                }
            }
            let position = self
                .compute_unit_config_position
                .unwrap_or(self.instructions.len());
            self.instructions
                .splice(position..position, budget_instructions);
        }
        if let Some((account, authority)) = self.nonce {
            self.instructions
                .insert(0, advance_nonce_account(&account, &authority));
        }
        let mut message = Message::new(&self.instructions, payer);
        if let Some(blockhash) = self.blockhash {
            message.recent_blockhash = blockhash;
        }
        VersionedMessage::Legacy(message)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_system_interface::instruction::transfer};

    #[test]
    fn builder_preserves_legacy_memo_budget_nonce_and_blockhash() {
        let payer = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let nonce_account = Pubkey::new_unique();
        let nonce_authority = Pubkey::new_unique();
        let blockhash = Hash::new_unique();
        let transfer = transfer(&payer, &recipient, 5);
        let config = ComputeUnitConfig {
            compute_unit_price: Some(7),
            compute_unit_limit: ComputeUnitLimit::Static(10_000),
        };

        let built = MessageBuilder::new(vec![transfer.clone()])
            .with_memo(Some("memo"))
            .with_compute_unit_config(&config)
            .with_nonce(&nonce_account, &nonce_authority)
            .with_blockhash(blockhash)
            .build(Some(&payer));

        let memo = Instruction {
            program_id: Pubkey::from(memo_program_id().to_bytes()),
            accounts: vec![],
            data: b"memo".to_vec(),
        };
        let mut expected = Message::new_with_nonce(
            vec![
                transfer,
                memo,
                ComputeBudgetInstruction::set_compute_unit_price(7),
                ComputeBudgetInstruction::set_compute_unit_limit(10_000),
            ],
            Some(&payer),
            &nonce_account,
            &nonce_authority,
        );
        expected.recent_blockhash = blockhash;
        assert_eq!(built, VersionedMessage::Legacy(expected));
    }
}
