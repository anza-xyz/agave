use {
    crate::compute_budget::ComputeUnitConfig,
    solana_clap_utils::compute_budget::ComputeUnitLimit,
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_message::{VersionedMessage, v1::TransactionConfig},
    solana_program_runtime::execution_budget::{
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT, MAX_COMPUTE_UNIT_LIMIT,
        MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
    },
    solana_pubkey::Pubkey,
    solana_system_interface::instruction::advance_nonce_account,
    spl_memo_interface::v4::id as memo_program_id,
};

/// Collects the instructions and transaction settings before compiling a CLI message.
pub(crate) struct MessageBuilder {
    instructions: Vec<Instruction>,
    compute_unit_price: Option<u64>,
    compute_unit_limit: ComputeUnitLimit,
    blockhash: Option<Hash>,
}

impl MessageBuilder {
    pub(crate) fn new(instructions: Vec<Instruction>) -> Self {
        Self {
            instructions,
            compute_unit_price: None,
            compute_unit_limit: ComputeUnitLimit::Default,
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
        self
    }

    pub(crate) fn with_instruction(mut self, instruction: Instruction) -> Self {
        self.instructions.push(instruction);
        self
    }

    pub(crate) fn with_nonce(mut self, account: &Pubkey, authority: &Pubkey) -> Self {
        self.instructions
            .insert(0, advance_nonce_account(account, authority));
        self
    }

    pub(crate) fn with_blockhash(mut self, blockhash: Hash) -> Self {
        self.blockhash = Some(blockhash);
        self
    }

    pub(crate) fn build(self, payer: &Pubkey) -> VersionedMessage {
        let compute_unit_limit = match self.compute_unit_limit {
            ComputeUnitLimit::Static(limit) => limit,
            ComputeUnitLimit::Simulated | ComputeUnitLimit::SimulatedWithExtraPercentage(_) => {
                MAX_COMPUTE_UNIT_LIMIT
            }
            ComputeUnitLimit::Default => u32::try_from(self.instructions.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
                .min(MAX_COMPUTE_UNIT_LIMIT),
        };
        let config = TransactionConfig {
            priority_fee: self
                .compute_unit_price
                .map(|price| priority_fee(price, compute_unit_limit)),
            compute_unit_limit: Some(compute_unit_limit),
            loaded_accounts_data_size_limit: Some(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get()),
            heap_size: None,
        };
        VersionedMessage::V1(
            solana_message::v1::Message::try_compile_with_config(
                payer,
                &self.instructions,
                self.blockhash.unwrap_or_default(),
                config,
            )
            .expect("CLI message compilation must succeed"),
        )
    }
}

pub(crate) fn priority_fee(price: u64, limit: u32) -> u64 {
    u128::from(price)
        .saturating_mul(u128::from(limit))
        .div_ceil(1_000_000)
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use {super::*, solana_system_interface::instruction::transfer};

    #[test]
    fn builder_places_nonce_first_and_uses_v1_config() {
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

        let built = MessageBuilder::new(vec![transfer])
            .with_memo(Some("memo"))
            .with_compute_unit_config(&config)
            .with_nonce(&nonce_account, &nonce_authority)
            .with_blockhash(blockhash)
            .build(&payer);

        let VersionedMessage::V1(message) = built else {
            panic!("expected a V1 message");
        };
        assert_eq!(message.lifetime_specifier, blockhash);
        assert_eq!(message.config.compute_unit_limit, Some(10_000));
        assert_eq!(message.config.priority_fee, Some(1));
        assert_eq!(message.instructions.len(), 3);
        let nonce_ix = advance_nonce_account(&nonce_account, &nonce_authority);
        assert_eq!(message.instructions[0].data, nonce_ix.data);
        assert_eq!(
            message.account_keys[usize::from(message.instructions[2].program_id_index)],
            Pubkey::from(memo_program_id().to_bytes())
        );
    }

    #[test]
    fn priority_fee_rounds_up_and_saturates() {
        assert_eq!(priority_fee(0, 1), 0);
        assert_eq!(priority_fee(1, 1), 1);
        assert_eq!(priority_fee(1_000_000, 1), 1);
        assert_eq!(priority_fee(1_000_001, 1), 2);
        assert_eq!(priority_fee(u64::MAX, u32::MAX), u64::MAX);
    }
}
