use {
    solana_account::{state_traits::StateMut, AccountSharedData, ReadableAccount},
    solana_clock::Epoch,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_stake_interface::{
        program as stake_program,
        stake_flags::StakeFlags,
        state::{Authorized, Delegation, Meta, Stake, StakeStateV2},
    },
    solana_vote_interface::state::VoteStateV4,
};

/// The minimum stake amount that can be delegated, in lamports.
/// When this feature is added, it will be accompanied by an upgrade to the BPF Stake Program.
/// NOTE: This is also used to calculate the minimum balance of a delegated stake account,
/// which is the rent exempt reserve _plus_ the minimum stake delegation.
#[inline(always)]
pub fn get_minimum_delegation(is_stake_raise_minimum_delegation_to_1_sol_active: bool) -> u64 {
    if is_stake_raise_minimum_delegation_to_1_sol_active {
        const MINIMUM_DELEGATION_SOL: u64 = 1;
        MINIMUM_DELEGATION_SOL * LAMPORTS_PER_SOL
    } else {
        1
    }
}

/// Utility function for creating a stake account from basic parameters.
/// Only used in tests and CLIs.
pub fn create_stake_account(
    authorized: &Pubkey,
    voter_pubkey: &Pubkey,
    vote_account: &AccountSharedData,
    rent: &Rent,
    lamports: u64,
) -> AccountSharedData {
    let mut stake_account =
        AccountSharedData::new(lamports, StakeStateV2::size_of(), &stake_program::id());

    let vote_state = VoteStateV4::deserialize(vote_account.data(), voter_pubkey).unwrap();
    let credits_observed = vote_state.credits();

    let rent_exempt_reserve = rent.minimum_balance(stake_account.data().len());
    let stake_amount = lamports
        .checked_sub(rent_exempt_reserve)
        .expect("lamports >= rent_exempt_reserve");

    let meta = Meta {
        authorized: Authorized::auto(authorized),
        rent_exempt_reserve,
        ..Meta::default()
    };

    let stake = Stake {
        delegation: Delegation::new(voter_pubkey, stake_amount, Epoch::MAX),
        credits_observed,
    };

    stake_account
        .set_state(&StakeStateV2::Stake(meta, stake, StakeFlags::empty()))
        .expect("set_state");

    stake_account
}
