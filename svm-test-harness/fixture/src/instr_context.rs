//! Instruction context (input).

use {
    agave_feature_set::FeatureSet, solana_account::Account, solana_pubkey::Pubkey,
    solana_stable_layout::stable_instruction::StableInstruction,
};

/// Instruction context fixture.
pub struct InstrContext {
    pub feature_set: FeatureSet,
    pub accounts: Vec<(Pubkey, Account)>,
    pub instruction: StableInstruction,
}

#[cfg(feature = "fuzz")]
use {
    crate::{error::FixtureError, proto::InstrContext as ProtoInstrContext},
    solana_instruction::AccountMeta,
};

#[cfg(feature = "fuzz")]
impl TryFrom<ProtoInstrContext> for InstrContext {
    type Error = FixtureError;

    fn try_from(value: ProtoInstrContext) -> Result<Self, Self::Error> {
        let program_id = Pubkey::new_from_array(
            value
                .program_id
                .try_into()
                .map_err(FixtureError::InvalidPubkeyBytes)?,
        );

        let feature_set: FeatureSet = value
            .epoch_context
            .as_ref()
            .and_then(|epoch_ctx| epoch_ctx.features.as_ref())
            .map(|fs| fs.into())
            .unwrap_or_default();

        let accounts: Vec<(Pubkey, Account)> = value
            .accounts
            .into_iter()
            .map(|acct_state| acct_state.try_into())
            .collect::<Result<Vec<_>, _>>()?;

        let instruction_accounts = value
            .instr_accounts
            .into_iter()
            .map(|acct| {
                if acct.index as usize >= accounts.len() {
                    return Err(FixtureError::AccountMissingForInstrAccount(
                        acct.index as usize,
                    ));
                }
                Ok(AccountMeta {
                    pubkey: accounts[acct.index as usize].0,
                    is_signer: acct.is_signer,
                    is_writable: acct.is_writable,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        if instruction_accounts.len() > 128 {
            return Err(FixtureError::InvalidFixtureInput);
        }

        let instruction = StableInstruction {
            accounts: instruction_accounts.into(),
            data: value.data.into(),
            program_id,
        };

        Ok(Self {
            feature_set,
            accounts,
            instruction,
        })
    }
}
