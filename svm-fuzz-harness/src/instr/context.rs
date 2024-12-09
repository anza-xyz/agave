//! Instruction context (inputs).

use {
    crate::{error::HarnessError, proto::InstrContext as ProtoInstrContext},
    solana_account::Account,
    solana_feature_set::FeatureSet,
    solana_hash::Hash,
    solana_instruction::AccountMeta,
    solana_pubkey::Pubkey,
    solana_sdk::rent_collector::RentCollector,
    solana_stable_layout::stable_instruction::StableInstruction,
};

pub struct InstrContext {
    pub feature_set: FeatureSet,
    pub accounts: Vec<(Pubkey, Account)>,
    pub instruction: StableInstruction,
    pub cu_avail: u64,
    pub rent_collector: RentCollector,
    pub last_blockhash: Hash,
    pub lamports_per_signature: u64,
}

impl TryFrom<ProtoInstrContext> for InstrContext {
    type Error = HarnessError;

    fn try_from(input: ProtoInstrContext) -> Result<Self, Self::Error> {
        let program_id = Pubkey::new_from_array(
            input
                .program_id
                .try_into()
                .map_err(|_| HarnessError::InvalidPubkeyBytes)?,
        );

        let feature_set: FeatureSet = input
            .epoch_context
            .as_ref()
            .and_then(|epoch_ctx| epoch_ctx.features.as_ref())
            .map(|fs| fs.into())
            .unwrap_or_default();

        let accounts: Vec<(Pubkey, Account)> = input
            .accounts
            .into_iter()
            .map(|acct_state| acct_state.try_into())
            .collect::<Result<Vec<_>, _>>()?;

        let instruction_accounts = input
            .instr_accounts
            .into_iter()
            .map(|acct| {
                if acct.index as usize >= accounts.len() {
                    return Err(HarnessError::AccountMissing);
                }
                Ok(AccountMeta {
                    pubkey: accounts[acct.index as usize].0,
                    is_signer: acct.is_signer,
                    is_writable: acct.is_writable,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let instruction = StableInstruction {
            accounts: instruction_accounts.into(),
            data: input.data.into(),
            program_id,
        };

        Ok(Self {
            feature_set,
            accounts,
            instruction,
            cu_avail: input.cu_avail,
            rent_collector: RentCollector::default(),
            last_blockhash: Hash::default(),
            lamports_per_signature: 0,
        })
    }
}
