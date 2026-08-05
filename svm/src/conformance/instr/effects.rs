//! Instruction effects (output).

#[cfg(feature = "conformance")]
use {
    crate::conformance::{account_state::account_to_proto, err::serialized_error_code},
    protosol::protos::InstrEffects as ProtoInstrEffects,
};
use {solana_account::Account, solana_instruction::error::InstructionError, solana_pubkey::Pubkey};

/// Represents the effects of a single instruction.
pub struct InstrEffects {
    pub result: Option<InstructionError>,
    pub custom_err: Option<u32>,
    pub resulting_accounts: Vec<(Pubkey, Account)>,
    pub cu_avail: u64,
    pub return_data: Vec<u8>,
    pub logs: Vec<String>,
}

impl InstrEffects {
    /// Returns the resulting account for the given pubkey, if it exists.
    pub fn get_account(&self, pubkey: &Pubkey) -> Option<&Account> {
        self.resulting_accounts
            .iter()
            .find(|(pk, _)| pk == pubkey)
            .map(|(_, acc)| acc)
    }
}

#[cfg(feature = "conformance")]
impl From<InstrEffects> for ProtoInstrEffects {
    fn from(value: InstrEffects) -> Self {
        let InstrEffects {
            result,
            custom_err,
            resulting_accounts,
            cu_avail,
            return_data,
            ..
        } = value;

        Self {
            result: result
                .as_ref()
                .map(|error| serialized_error_code(error) as i32)
                .unwrap_or_default(),
            custom_err: custom_err.unwrap_or_default(),
            modified_accounts: resulting_accounts
                .into_iter()
                .map(account_to_proto)
                .collect(),
            cu_avail,
            return_data,
        }
    }
}
