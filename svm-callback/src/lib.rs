#![cfg(feature = "agave-unstable-api")]
use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_precompile_error::PrecompileError,
    solana_pubkey::Pubkey,
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

    fn get_leader_info(&self) -> LeaderInfo {
        LeaderInfo::default()
    }
}

/// Account loading for SVM.
pub trait AccountLoadCallback {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData>;
}

/// Runtime callbacks for transaction processing.
pub trait TransactionProcessingCallback: AccountLoadCallback {
    fn get_account_shared_data_if(
        &self,
        pubkey: &Pubkey,
        load_filter: impl Fn(u64, &Pubkey, usize) -> bool,
    ) -> Option<AccountSharedData> {
        self.get_account_shared_data(pubkey).filter(|account| {
            load_filter(account.lamports(), account.owner(), account.data().len())
        })
    }
}

#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LeaderInfo {
    pub leader_id: Pubkey,
    pub next_leader_id: Pubkey,
    pub leader_vote: Pubkey,
    pub next_leader_vote: Pubkey,
}
