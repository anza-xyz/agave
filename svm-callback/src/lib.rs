use {solana_account::AccountSharedData, solana_pubkey::Pubkey};

/// Callback for obtaining the cluster's current epoch stake.
pub trait EpochStakeCallback {
    /// Returns the total current epoch stake for the network.
    fn get_epoch_stake(&self) -> u64 {
        0
    }

    /// Returns the current epoch stake for the given vote account.
    fn get_epoch_stake_for_vote_account(&self, _vote_address: &Pubkey) -> u64 {
        0
    }
}

/// Runtime callbacks for transaction processing.
pub trait TransactionProcessingCallback: EpochStakeCallback {
    fn account_matches_owners(&self, account: &Pubkey, owners: &[Pubkey]) -> Option<usize>;

    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData>;

    fn add_builtin_account(&self, _name: &str, _program_id: &Pubkey) {}

    fn inspect_account(&self, _address: &Pubkey, _account_state: AccountState, _is_writable: bool) {
    }

    #[deprecated(
        since = "2.3.0",
        note = "Use `get_epoch_stake_for_vote_account` on the `EpochStakeCallback` trait instead"
    )]
    fn get_current_epoch_vote_account_stake(&self, vote_address: &Pubkey) -> u64 {
        Self::get_epoch_stake_for_vote_account(self, vote_address)
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
