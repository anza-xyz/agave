/// utility function used for testing and benchmarking.
use {
    super::Bank,
    crate::installed_scheduler_pool::BankWithScheduler,
    solana_sdk::{
        account::{ReadableAccount, WritableAccount},
        hash::hashv,
        lamports::LamportsError,
        pubkey::Pubkey,
    },
    solana_vote_program::vote_state::{self, BlockTimestamp, VoteStateVersions},
    std::sync::Arc,
};

pub fn goto_end_of_slot(bank: Arc<Bank>) {
    goto_end_of_slot_with_scheduler(&BankWithScheduler::new_without_scheduler(bank))
}

pub fn goto_end_of_slot_with_scheduler(bank: &BankWithScheduler) {
    let mut tick_hash = bank.last_blockhash();
    loop {
        tick_hash = hashv(&[tick_hash.as_ref(), &[42]]);
        bank.register_tick(&tick_hash);
        if tick_hash == bank.last_blockhash() {
            bank.freeze();
            return;
        }
    }
}

pub fn update_vote_account_timestamp(timestamp: BlockTimestamp, bank: &Bank, vote_pubkey: &Pubkey) {
    let mut vote_account = bank.get_account(vote_pubkey).unwrap_or_default();
    let mut vote_state = vote_state::from(&vote_account).unwrap_or_default();
    vote_state.last_timestamp = timestamp;
    let versioned = VoteStateVersions::new_current(vote_state);
    vote_state::to(&versioned, &mut vote_account).unwrap();
    bank.store_account(vote_pubkey, &vote_account);
}

pub fn deposit(
    bank: &Bank,
    pubkey: &Pubkey,
    lamports: u64,
) -> std::result::Result<u64, LamportsError> {
    // This doesn't collect rents intentionally.
    // Rents should only be applied to actual TXes
    let mut account = bank
        .get_account_with_fixed_root_no_cache(pubkey)
        .unwrap_or_default();
    account.checked_add_lamports(lamports)?;
    bank.store_account(pubkey, &account);
    Ok(account.lamports())
}
