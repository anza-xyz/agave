#![cfg(feature = "agave-unstable-api")]
use {
    solana_account::AccountSharedData, solana_precompile_error::PrecompileError,
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

    /// Returns the leaders of the current and next slot for the
    /// `sol_get_leader` syscall.
    fn get_leader_info(&self) -> LeaderInfo {
        LeaderInfo::default()
    }
}

/// Runtime callbacks for transaction processing.
pub trait TransactionProcessingCallback {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData>;
}

/// The block producer of the current slot and of the slot after it, as written
/// by the `sol_get_leader` syscall (SIMD-0558). This is the syscall's ABI:
/// four pubkeys, 128 bytes, in this order.
#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LeaderInfo {
    pub leader_identity: Pubkey,
    pub leader_vote: Pubkey,
    pub next_leader_identity: Pubkey,
    pub next_leader_vote: Pubkey,
}
