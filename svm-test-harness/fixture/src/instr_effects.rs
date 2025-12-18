//! Instruction effects (output).

use {solana_account::Account, solana_instruction_error::InstructionError, solana_pubkey::Pubkey};

/// Represents the effects of a single instruction.
pub struct InstrEffects {
    pub result: Option<InstructionError>,
    pub custom_err: Option<u32>,
    pub modified_accounts: Vec<(Pubkey, Account)>,
    pub cu_avail: u64,
    pub return_data: Vec<u8>,
    pub logs: Vec<String>,
}

impl InstrEffects {
    /// Returns the modified account for the given pubkey, if it exists.
    pub fn get_account(&self, pubkey: &Pubkey) -> Option<&Account> {
        self.modified_accounts
            .iter()
            .find(|(pk, _)| pk == pubkey)
            .map(|(_, acc)| acc)
    }
}

#[cfg(feature = "fuzz")]
use {crate::proto::InstrEffects as ProtoInstrEffects, bincode};

#[cfg(feature = "fuzz")]
impl From<InstrEffects> for ProtoInstrEffects {
    fn from(value: InstrEffects) -> Self {
        let InstrEffects {
            result,
            custom_err,
            modified_accounts,
            cu_avail,
            return_data,
            ..
        } = value;

        Self {
            result: result
                .as_ref()
                .map(|error| {
                    let serialized_err = bincode::serialize(error).unwrap();
                    i32::from_le_bytes((&serialized_err[0..4]).try_into().unwrap())
                        .saturating_add(1)
                })
                .unwrap_or_default(),
            custom_err: custom_err.unwrap_or_default(),
            modified_accounts: modified_accounts.into_iter().map(Into::into).collect(),
            cu_avail,
            return_data,
        }
    }
}
