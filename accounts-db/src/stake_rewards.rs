//! Code for stake and vote rewards

use {
    crate::{
        is_zero_lamport::IsZeroLamport,
        storable_accounts::{AccountForStorage, StorableAccounts},
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_reward_info::RewardType,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StakeRewardInfo {
    pub reward_type: RewardType,
    pub lamports: i64,
    pub post_balance: u64,
    pub commission_bps: Option<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StakeReward {
    pub stake_pubkey: Pubkey,
    pub stake_reward_info: StakeRewardInfo,
    pub stake_account: AccountSharedData,
}

impl StakeReward {
    pub fn get_stake_reward(&self) -> i64 {
        self.stake_reward_info.lamports
    }
}

impl IsZeroLamport for StakeReward {
    fn is_zero_lamport(&self) -> bool {
        self.stake_account.lamports() == 0
    }
}

/// allow [StakeReward] to be passed to `StoreAccounts` directly without copies or vec construction
impl<'a> StorableAccounts<'a> for (Slot, &'a [StakeReward]) {
    fn account<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret {
        let entry = &self.1[index];
        callback((&entry.stake_pubkey, &entry.stake_account).into())
    }
    fn account_for_geyser<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(&'local Pubkey, &'local AccountSharedData) -> Ret,
    ) -> Ret {
        let entry = &self.1[index];
        callback(&entry.stake_pubkey, &entry.stake_account)
    }
    fn is_zero_lamport(&self, index: usize) -> bool {
        self.1[index].is_zero_lamport()
    }
    fn data_len(&self, index: usize) -> usize {
        self.1[index].stake_account.data().len()
    }
    fn pubkey(&self, index: usize) -> &Pubkey {
        &self.1[index].stake_pubkey
    }
    fn slot(&self, _index: usize) -> Slot {
        // per-index slot is not unique per slot when per-account slot is not included in the source data
        self.target_slot()
    }
    fn target_slot(&self) -> Slot {
        self.0
    }
    fn len(&self) -> usize {
        self.1.len()
    }
}

#[cfg(feature = "dev-context-only-utils")]
use {
    rand::Rng,
    solana_account::{WritableAccount, state_traits::StateMut},
    solana_clock::Epoch,
    solana_keypair::Keypair,
    solana_rent::Rent,
    solana_signer::Signer,
    solana_stake_interface::{
        program as stake_program,
        stake_flags::StakeFlags,
        state::{Authorized, Delegation, Meta, Stake, StakeStateV2},
    },
    solana_vote_program::vote_state::{self, BLS_PUBLIC_KEY_COMPRESSED_SIZE},
};

// These functions/fields are only usable from a dev context (i.e. tests and benches)
#[cfg(feature = "dev-context-only-utils")]
impl StakeReward {
    pub fn new_random(rent: &Rent) -> Self {
        let mut rng = rand::rng();

        let validator_pubkey = solana_pubkey::new_rand();
        let validator_stake_lamports = 20;
        let validator_staking_keypair = Keypair::new();
        let validator_voting_keypair = Keypair::new();

        let validator_vote_account = vote_state::create_v4_account_with_authorized(
            &validator_pubkey,
            &validator_voting_keypair.pubkey(),
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &validator_voting_keypair.pubkey(),
            1000,
            &validator_voting_keypair.pubkey(),
            0,
            &validator_voting_keypair.pubkey(),
            validator_stake_lamports,
        );

        let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
        let reward_lamports: i64 = rng.random_range(1..200);
        let validator_stake_account = create_stake_account(
            &validator_staking_keypair.pubkey(),
            &validator_voting_keypair.pubkey(),
            &validator_vote_account,
            rent,
            rent_exempt_reserve + validator_stake_lamports + reward_lamports as u64,
        );

        Self {
            stake_pubkey: Pubkey::new_unique(),
            stake_reward_info: StakeRewardInfo {
                reward_type: solana_reward_info::RewardType::Staking,
                lamports: reward_lamports,
                post_balance: 0,         /* unused atm */
                commission_bps: Some(0), /* unused but tests require some value */
            },

            stake_account: validator_stake_account,
        }
    }

    pub fn new_with_pre_stake_account(
        reward_lamports: i64,
        stake_lamports: u64,
        rent: &Rent,
    ) -> (AccountSharedData, Self) {
        let vote_pubkey = Pubkey::new_unique();
        let vote_account = vote_state::create_v4_account_with_authorized(
            &Pubkey::new_unique(),
            &vote_pubkey,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &vote_pubkey,
            1000,
            &vote_pubkey,
            0,
            &vote_pubkey,
            stake_lamports,
        );

        let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
        let stake_pubkey = Pubkey::new_unique();
        let pre_stake_account = create_stake_account(
            &stake_pubkey,
            &vote_pubkey,
            &vote_account,
            rent,
            rent_exempt_reserve + stake_lamports,
        );
        let post_stake_account = create_stake_account(
            &stake_pubkey,
            &vote_pubkey,
            &vote_account,
            rent,
            rent_exempt_reserve + stake_lamports + reward_lamports as u64,
        );

        (
            pre_stake_account,
            Self {
                stake_pubkey,
                stake_reward_info: StakeRewardInfo {
                    reward_type: solana_reward_info::RewardType::Staking,
                    lamports: reward_lamports,
                    post_balance: 0,         /* unused atm */
                    commission_bps: Some(0), /* unused but tests require some value */
                },

                stake_account: post_stake_account,
            },
        )
    }

    pub fn credit(&mut self, amount: u64) {
        self.stake_reward_info.lamports = amount as i64;
        self.stake_reward_info.post_balance += amount;
        self.stake_account.checked_add_lamports(amount).unwrap();
    }
}

#[cfg(feature = "dev-context-only-utils")]
fn create_stake_account(
    authorized: &Pubkey,
    voter_pubkey: &Pubkey,
    vote_account: &AccountSharedData,
    rent: &Rent,
    lamports: u64,
) -> AccountSharedData {
    let mut stake_account =
        AccountSharedData::new(lamports, StakeStateV2::size_of(), &stake_program::id());

    let vote_state =
        vote_state::VoteStateV4::deserialize(vote_account.data(), voter_pubkey).unwrap();
    let credits_observed = vote_state.credits();
    let last_epoch = vote_state.epoch_credits.last().map(|c| c.0).unwrap_or(0);

    let rent_exempt_reserve = rent.minimum_balance(stake_account.data().len());
    let stake_amount = lamports
        .checked_sub(rent_exempt_reserve)
        .expect("lamports >= rent_exempt_reserve");

    let meta = Meta {
        authorized: Authorized::auto(authorized),
        rent_exempt_reserve,
        ..Meta::default()
    };

    let mut stake = Stake {
        delegation: Delegation::new(voter_pubkey, stake_amount, Epoch::MAX),
        credits_observed,
    };

    if stake_amount == 0 {
        stake.delegation.deactivation_epoch = last_epoch;
    }

    stake_account
        .set_state(&StakeStateV2::Stake(meta, stake, StakeFlags::empty()))
        .expect("set_state");

    stake_account
}
