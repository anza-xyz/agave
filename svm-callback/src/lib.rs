#![cfg(feature = "agave-unstable-api")]
use {
    solana_account::AccountSharedData, solana_clock::Slot,
    solana_precompile_error::PrecompileError, solana_pubkey::Pubkey,
};

/// Callback used by InvokeContext in SVM
pub trait InvokeContextCallback {
    /// Returns the total current epoch stake for the network.
    fn get_epoch_stake(&self) -> u64 {
        0
    }

    /// Returns the current epoch stake for the given vote account.
    fn get_epoch_stake_for_vote_account(&self, _vote_address: &Pubkey) -> u64 {
        0
    }

    /// Returns true if the program_id corresponds to a precompiled program
    fn is_precompile(&self, _program_id: &Pubkey) -> bool {
        false
    }

    /// Calls the precompiled program corresponding to the given program ID.
    fn process_precompile(
        &self,
        _program_id: &Pubkey,
        _data: &[u8],
        _instruction_datas: Vec<&[u8]>,
    ) -> Result<(), PrecompileError> {
        Err(PrecompileError::InvalidPublicKey)
    }
}

/// Runtime callbacks for transaction processing.
pub trait TransactionProcessingCallback: InvokeContextCallback {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)>;

    fn inspect_account(&self, _address: &Pubkey, _account_state: AccountState, _is_writable: bool) {
    }
}

/// The state the account is in initially, before transaction processing
#[derive(Debug)]
pub enum AccountState<'a> {
    /// This account is dead, and will be created by this transaction
    Dead,
    /// This account is alive, and already existed prior to this transaction
    Alive(&'a AccountSharedData),
}
