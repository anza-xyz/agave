//! Code for stake and vote rewards

use {
    crate::{
        is_zero_lamport::IsZeroLamport,
        storable_accounts::{AccountForStorage, StorableAccounts},
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_reward_info::RewardInfo,
};

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct StakeReward {
    pub stake_pubkey: Pubkey,
    pub stake_reward_info: RewardInfo,
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
        callback((&self.1[index].stake_pubkey, &entry.stake_account).into())
    }
    fn is_zero_lamport(&self, index: usize) -> bool {
        self.1[index].is_zero_lamport()
    }
    fn data_len(&self, index: usize) -> usize {
        self.1[index].stake_account.data().len()
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
    rand::Rng, solana_account::WritableAccount, solana_keypair::Keypair, solana_rent::Rent,
    solana_signer::Signer, solana_stake_program::stake_state, solana_vote_program::vote_state,
};

// These functions/fields are only usable from a dev context (i.e. tests and benches)
#[cfg(feature = "dev-context-only-utils")]
impl StakeReward {
    pub fn new_random() -> Self {
        let mut rng = rand::thread_rng();

        let rent = Rent::free();

        let validator_pubkey = solana_pubkey::new_rand();
        let validator_stake_lamports = 20;
        let validator_staking_keypair = Keypair::new();
        let validator_voting_keypair = Keypair::new();

        let validator_vote_account = vote_state::create_account(
            &validator_voting_keypair.pubkey(),
            &validator_pubkey,
            10,
            validator_stake_lamports,
        );

        let reward_lamports: i64 = rng.gen_range(1..200);
        let validator_stake_account = stake_state::create_account(
            &validator_staking_keypair.pubkey(),
            &validator_voting_keypair.pubkey(),
            &validator_vote_account,
            &rent,
            validator_stake_lamports + reward_lamports as u64,
        );

        Self {
            stake_pubkey: Pubkey::new_unique(),
            stake_reward_info: RewardInfo {
                reward_type: solana_reward_info::RewardType::Staking,
                lamports: reward_lamports,
                post_balance: 0,     /* unused atm */
                commission: Some(0), /* unused but tests require some value */
            },

            stake_account: validator_stake_account,
        }
    }

    pub fn credit(&mut self, amount: u64) {
        self.stake_reward_info.lamports = amount as i64;
        self.stake_reward_info.post_balance += amount;
        self.stake_account.checked_add_lamports(amount).unwrap();
    }
}
