mod calculation;
mod distribution;
mod epoch_rewards_hasher;
mod sysvar;

use {
    super::Bank,
    crate::{
        inflation_rewards::points::PointValue, stake_account::StakeAccount,
        stake_history::StakeHistory,
    },
    rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        partitioned_rewards::PartitionedEpochRewardsConfig,
        stake_rewards::StakeReward,
        storable_accounts::{AccountForStorage, StorableAccounts},
    },
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_reward_info::RewardInfo,
    solana_stake_interface::state::{Delegation, Stake},
    solana_vote::vote_account::VoteAccounts,
    std::{mem::MaybeUninit, sync::Arc},
};

/// Number of blocks for reward calculation and storing vote accounts.
/// Distributing rewards to stake accounts begins AFTER this many blocks.
const REWARD_CALCULATION_NUM_BLOCKS: u64 = 1;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartitionedStakeReward {
    /// Stake account address
    pub stake_pubkey: Pubkey,
    /// `Stake` state to be stored in account
    pub stake: Stake,
    /// Stake reward for recording in the Bank on distribution
    pub stake_reward: u64,
    /// Vote commission for recording reward info
    pub commission: u8,
}

/// A vector of stake rewards.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PartitionedStakeRewards {
    /// Inner vector.
    rewards: Vec<Option<PartitionedStakeReward>>,
    /// Number of stake rewards.
    num_rewards: usize,
}

impl PartitionedStakeRewards {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let rewards = Vec::with_capacity(capacity);
        Self {
            rewards,
            num_rewards: 0,
        }
    }

    /// Number of stake rewards.
    pub(crate) fn num_rewards(&self) -> usize {
        self.num_rewards
    }

    /// Total length, including both `Some` and `None` elements.
    pub(crate) fn total_len(&self) -> usize {
        self.rewards.len()
    }

    pub(crate) fn get(&self, index: usize) -> Option<&Option<PartitionedStakeReward>> {
        self.rewards.get(index)
    }

    pub(crate) fn enumerated_rewards_iter(
        &self,
    ) -> impl Iterator<Item = (usize, &PartitionedStakeReward)> {
        self.rewards
            .iter()
            .enumerate()
            .filter_map(|(index, reward)| reward.as_ref().map(|reward| (index, reward)))
    }

    fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<Option<PartitionedStakeReward>>] {
        self.rewards.spare_capacity_mut()
    }

    unsafe fn assume_init(&mut self, num_stake_rewards: usize) {
        unsafe {
            self.rewards.set_len(self.rewards.capacity());
        }
        self.num_rewards = num_stake_rewards;
    }
}

#[cfg(test)]
impl FromIterator<Option<PartitionedStakeReward>> for PartitionedStakeRewards {
    fn from_iter<T: IntoIterator<Item = Option<PartitionedStakeReward>>>(iter: T) -> Self {
        let mut len_some: usize = 0;
        let rewards = Vec::from_iter(iter.into_iter().inspect(|reward| {
            if reward.is_some() {
                len_some = len_some.saturating_add(1);
            }
        }));
        Self {
            rewards,
            num_rewards: len_some,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StartBlockHeightAndRewards {
    /// the block height of the slot at which rewards distribution began
    pub(crate) distribution_starting_block_height: u64,
    /// calculated epoch rewards before partitioning
    pub(crate) all_stake_rewards: Arc<PartitionedStakeRewards>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StartBlockHeightAndPartitionedRewards {
    /// the block height of the slot at which rewards distribution began
    pub(crate) distribution_starting_block_height: u64,

    /// calculated epoch rewards pending distribution
    pub(crate) all_stake_rewards: Arc<PartitionedStakeRewards>,

    /// indices of calculated epoch rewards per partition, outer Vec is by
    /// partition (one partition per block), inner Vec is the indices for one
    /// partition.
    pub(crate) partition_indices: Vec<Vec<usize>>,
}

/// Represent whether bank is in the reward phase or not.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum EpochRewardStatus {
    /// this bank is in the reward phase.
    /// Contents are the start point for epoch reward calculation,
    /// i.e. parent_slot and parent_block height for the starting
    /// block of the current epoch.
    Active(EpochRewardPhase),
    /// this bank is outside of the rewarding phase.
    #[default]
    Inactive,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EpochRewardPhase {
    Calculation(StartBlockHeightAndRewards),
    Distribution(StartBlockHeightAndPartitionedRewards),
}

#[derive(Debug, Default)]
pub(super) struct VoteRewardsAccounts {
    /// accounts with rewards to be stored
    pub(super) accounts_with_rewards: Vec<(Pubkey, RewardInfo, AccountSharedData)>,
    /// total lamports across all `vote_rewards`
    pub(super) total_vote_rewards_lamports: u64,
}

/// Wrapper struct to implement StorableAccounts for VoteRewardsAccounts
pub(super) struct VoteRewardsAccountsStorable<'a> {
    pub slot: Slot,
    pub vote_rewards_accounts: &'a VoteRewardsAccounts,
}

impl<'a> StorableAccounts<'a> for VoteRewardsAccountsStorable<'a> {
    fn account<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret {
        let (pubkey, _, account) = &self.vote_rewards_accounts.accounts_with_rewards[index];
        callback((pubkey, account).into())
    }

    fn is_zero_lamport(&self, index: usize) -> bool {
        self.vote_rewards_accounts.accounts_with_rewards[index]
            .2
            .lamports()
            == 0
    }

    fn data_len(&self, index: usize) -> usize {
        self.vote_rewards_accounts.accounts_with_rewards[index]
            .2
            .data()
            .len()
    }

    fn pubkey(&self, index: usize) -> &Pubkey {
        &self.vote_rewards_accounts.accounts_with_rewards[index].0
    }

    fn slot(&self, _index: usize) -> Slot {
        self.target_slot()
    }

    fn target_slot(&self) -> Slot {
        self.slot
    }

    fn len(&self) -> usize {
        self.vote_rewards_accounts.accounts_with_rewards.len()
    }
}

#[derive(Debug, Default)]
/// result of calculating the stake rewards at end of epoch
pub(super) struct StakeRewardCalculation {
    /// each individual stake account to reward
    stake_rewards: Arc<PartitionedStakeRewards>,
    /// total lamports across all `stake_rewards`
    total_stake_rewards_lamports: u64,
}

#[derive(Debug)]
struct CalculateValidatorRewardsResult {
    vote_rewards_accounts: VoteRewardsAccounts,
    stake_reward_calculation: StakeRewardCalculation,
    point_value: PointValue,
}

impl Default for CalculateValidatorRewardsResult {
    fn default() -> Self {
        Self {
            vote_rewards_accounts: VoteRewardsAccounts::default(),
            stake_reward_calculation: StakeRewardCalculation::default(),
            point_value: PointValue {
                points: 0,
                rewards: 0,
            },
        }
    }
}

pub(super) struct FilteredStakeDelegations<'a> {
    stake_delegations: Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)>,
    min_stake_delegation: Option<u64>,
}

impl<'a> FilteredStakeDelegations<'a> {
    pub(super) fn len(&self) -> usize {
        self.stake_delegations.len()
    }

    pub(super) fn par_iter(
        &'a self,
    ) -> impl IndexedParallelIterator<Item = Option<(&'a Pubkey, &'a StakeAccount<Delegation>)>>
    {
        self.stake_delegations
            .par_iter()
            // We yield `None` items instead of filtering them out to
            // keep the number of elements predictable. It's better to
            // let the callers deal with `None` elements and even store
            // them in collections (that are allocated once with the
            // size of `FilteredStakeDelegations::len`) rather than
            // `collect` yet another time (which would take ~100ms).
            .map(|(pubkey, stake_account)| {
                match self.min_stake_delegation {
                    Some(min_stake_delegation)
                        if stake_account.delegation().stake < min_stake_delegation =>
                    {
                        None
                    }
                    _ => {
                        // Dereference `&&` to `&`.
                        Some((*pubkey, *stake_account))
                    }
                }
            })
    }
}

/// hold reward calc info to avoid recalculation across functions
pub(super) struct EpochRewardCalculateParamInfo<'a> {
    pub(super) stake_history: StakeHistory,
    pub(super) stake_delegations: FilteredStakeDelegations<'a>,
    pub(super) cached_vote_accounts: &'a VoteAccounts,
}

/// Hold all results from calculating the rewards for partitioned distribution.
/// This struct exists so we can have a function which does all the calculation with no
/// side effects.
#[derive(Debug)]
pub(super) struct PartitionedRewardsCalculation {
    pub(super) vote_account_rewards: VoteRewardsAccounts,
    pub(super) stake_rewards: StakeRewardCalculation,
    pub(super) validator_rate: f64,
    pub(super) foundation_rate: f64,
    pub(super) prev_epoch_duration_in_years: f64,
    pub(super) capitalization: u64,
    point_value: PointValue,
}

pub(crate) type StakeRewards = Vec<StakeReward>;

#[derive(Debug, PartialEq)]
pub struct KeyedRewardsAndNumPartitions {
    pub keyed_rewards: Vec<(Pubkey, RewardInfo)>,
    pub num_partitions: Option<u64>,
}

impl KeyedRewardsAndNumPartitions {
    pub fn should_record(&self) -> bool {
        !self.keyed_rewards.is_empty() || self.num_partitions.is_some()
    }
}

impl Bank {
    pub fn get_rewards_and_num_partitions(&self) -> KeyedRewardsAndNumPartitions {
        let keyed_rewards = self.rewards.read().unwrap().clone();
        let epoch_rewards_sysvar = self.get_epoch_rewards_sysvar();
        // If partitioned epoch rewards are active and this Bank is the
        // epoch-boundary block, populate num_partitions
        let epoch_schedule = self.epoch_schedule();
        let parent_epoch = epoch_schedule.get_epoch(self.parent_slot());
        let is_first_block_in_epoch = self.epoch() > parent_epoch;

        let num_partitions = (epoch_rewards_sysvar.active && is_first_block_in_epoch)
            .then_some(epoch_rewards_sysvar.num_partitions);
        KeyedRewardsAndNumPartitions {
            keyed_rewards,
            num_partitions,
        }
    }

    pub(crate) fn set_epoch_reward_status_calculation(
        &mut self,
        distribution_starting_block_height: u64,
        stake_rewards: Arc<PartitionedStakeRewards>,
    ) {
        self.epoch_reward_status =
            EpochRewardStatus::Active(EpochRewardPhase::Calculation(StartBlockHeightAndRewards {
                distribution_starting_block_height,
                all_stake_rewards: stake_rewards,
            }));
    }

    pub(crate) fn set_epoch_reward_status_distribution(
        &mut self,
        distribution_starting_block_height: u64,
        all_stake_rewards: Arc<PartitionedStakeRewards>,
        partition_indices: Vec<Vec<usize>>,
    ) {
        self.epoch_reward_status = EpochRewardStatus::Active(EpochRewardPhase::Distribution(
            StartBlockHeightAndPartitionedRewards {
                distribution_starting_block_height,
                all_stake_rewards,
                partition_indices,
            },
        ));
    }

    pub(super) fn partitioned_epoch_rewards_config(&self) -> &PartitionedEpochRewardsConfig {
        &self
            .rc
            .accounts
            .accounts_db
            .partitioned_epoch_rewards_config
    }

    /// # stake accounts to store in one block during partitioned reward interval
    pub(super) fn partitioned_rewards_stake_account_stores_per_block(&self) -> u64 {
        self.partitioned_epoch_rewards_config()
            .stake_account_stores_per_block
    }

    /// Calculate the number of blocks required to distribute rewards to all stake accounts.
    pub(super) fn get_reward_distribution_num_blocks(
        &self,
        rewards: &PartitionedStakeRewards,
    ) -> u64 {
        let total_stake_accounts = rewards.num_rewards();
        if self.epoch_schedule.warmup && self.epoch < self.first_normal_epoch() {
            1
        } else {
            const MAX_FACTOR_OF_REWARD_BLOCKS_IN_EPOCH: u64 = 10;
            let num_chunks = total_stake_accounts
                .div_ceil(self.partitioned_rewards_stake_account_stores_per_block() as usize)
                as u64;

            // Limit the reward credit interval to 10% of the total number of slots in a epoch
            num_chunks.clamp(
                1,
                (self.epoch_schedule.slots_per_epoch / MAX_FACTOR_OF_REWARD_BLOCKS_IN_EPOCH).max(1),
            )
        }
    }

    /// For testing only
    pub fn force_reward_interval_end_for_tests(&mut self) {
        self.epoch_reward_status = EpochRewardStatus::Inactive;
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::tests::create_genesis_config,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_vote_accounts, GenesisConfigInfo, ValidatorVoteKeypairs,
            },
            runtime_config::RuntimeConfig,
            stake_utils,
        },
        assert_matches::assert_matches,
        solana_account::{state_traits::StateMut, Account},
        solana_accounts_db::accounts_db::{AccountsDbConfig, ACCOUNTS_DB_CONFIG_FOR_TESTING},
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_reward_info::RewardType,
        solana_signer::Signer,
        solana_stake_interface::state::StakeStateV2,
        solana_system_transaction as system_transaction,
        solana_vote::vote_transaction,
        solana_vote_interface::state::{VoteStateV4, VoteStateVersions, MAX_LOCKOUT_HISTORY},
        solana_vote_program::vote_state::{self, handler::VoteStateHandle, TowerSync},
        std::sync::{Arc, RwLock},
    };

    impl PartitionedStakeReward {
        fn maybe_from(stake_reward: &StakeReward) -> Option<Self> {
            if let Ok(StakeStateV2::Stake(_meta, stake, _flags)) =
                stake_reward.stake_account.state()
            {
                Some(Self {
                    stake_pubkey: stake_reward.stake_pubkey,
                    stake,
                    stake_reward: stake_reward.stake_reward_info.lamports as u64,
                    commission: stake_reward.stake_reward_info.commission.unwrap(),
                })
            } else {
                None
            }
        }

        pub fn new_random() -> Self {
            Self::maybe_from(&StakeReward::new_random()).unwrap()
        }
    }

    pub fn build_partitioned_stake_rewards(
        stake_rewards: &PartitionedStakeRewards,
        partition_indices: &[Vec<usize>],
    ) -> Vec<PartitionedStakeRewards> {
        partition_indices
            .iter()
            .map(|partition_index| {
                // partition_index is a Vec<usize> that contains the indices of the stake rewards
                // that belong to this partition
                partition_index
                    .iter()
                    .map(|&index| stake_rewards.get(index).unwrap().clone())
                    .collect::<PartitionedStakeRewards>()
            })
            .collect::<Vec<_>>()
    }

    pub fn convert_rewards(
        stake_rewards: impl IntoIterator<Item = StakeReward>,
    ) -> PartitionedStakeRewards {
        stake_rewards
            .into_iter()
            .map(|stake_reward| Some(PartitionedStakeReward::maybe_from(&stake_reward).unwrap()))
            .collect()
    }

    #[derive(Debug, PartialEq, Eq, Copy, Clone)]
    enum RewardInterval {
        /// the slot within the epoch is INSIDE the reward distribution interval
        InsideInterval,
        /// the slot within the epoch is OUTSIDE the reward distribution interval
        OutsideInterval,
    }

    impl Bank {
        /// Return `RewardInterval` enum for current bank
        fn get_reward_interval(&self) -> RewardInterval {
            if matches!(self.epoch_reward_status, EpochRewardStatus::Active(_)) {
                RewardInterval::InsideInterval
            } else {
                RewardInterval::OutsideInterval
            }
        }

        fn is_calculated(&self) -> bool {
            matches!(
                self.epoch_reward_status,
                EpochRewardStatus::Active(EpochRewardPhase::Calculation(_))
            )
        }

        fn is_partitioned(&self) -> bool {
            matches!(
                self.epoch_reward_status,
                EpochRewardStatus::Active(EpochRewardPhase::Distribution(_))
            )
        }

        fn get_epoch_rewards_from_cache(
            &self,
            parent_hash: &Hash,
        ) -> Option<Arc<PartitionedRewardsCalculation>> {
            self.epoch_rewards_calculation_cache
                .lock()
                .unwrap()
                .get(parent_hash)
                .cloned()
        }

        fn get_epoch_rewards_cache_len(&self) -> usize {
            self.epoch_rewards_calculation_cache.lock().unwrap().len()
        }
    }

    pub(super) const SLOTS_PER_EPOCH: u64 = 32;

    pub(super) struct RewardBank {
        pub(super) bank: Arc<Bank>,
        pub(super) voters: Vec<Pubkey>,
        pub(super) stakers: Vec<Pubkey>,
    }

    /// Helper functions to create a bank that pays some rewards
    pub(super) fn create_default_reward_bank(
        expected_num_delegations: usize,
        advance_num_slots: u64,
    ) -> (RewardBank, Arc<RwLock<BankForks>>) {
        create_reward_bank(
            expected_num_delegations,
            PartitionedEpochRewardsConfig::default().stake_account_stores_per_block,
            advance_num_slots,
        )
    }

    pub(super) fn create_reward_bank(
        expected_num_delegations: usize,
        stake_account_stores_per_block: u64,
        advance_num_slots: u64,
    ) -> (RewardBank, Arc<RwLock<BankForks>>) {
        create_reward_bank_with_specific_stakes(
            vec![2_000_000_000; expected_num_delegations],
            stake_account_stores_per_block,
            advance_num_slots,
        )
    }

    pub(super) fn create_reward_bank_with_specific_stakes(
        stakes: Vec<u64>,
        stake_account_stores_per_block: u64,
        advance_num_slots: u64,
    ) -> (RewardBank, Arc<RwLock<BankForks>>) {
        let validator_keypairs = (0..stakes.len())
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();

        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config_with_vote_accounts(1_000_000_000, &validator_keypairs, stakes);
        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);

        let mut accounts_db_config: AccountsDbConfig = ACCOUNTS_DB_CONFIG_FOR_TESTING.clone();
        accounts_db_config.partitioned_epoch_rewards_config =
            PartitionedEpochRewardsConfig::new_for_test(stake_account_stores_per_block);

        let bank = Bank::new_from_genesis(
            &genesis_config,
            Arc::new(RuntimeConfig::default()),
            Vec::new(),
            None,
            accounts_db_config,
            None,
            Some(Pubkey::new_unique()),
            Arc::default(),
            None,
            None,
        );

        // Fill bank_forks with banks with votes landing in the next slot
        // Create enough banks such that vote account will root
        populate_vote_accounts_with_votes(
            &bank,
            validator_keypairs.iter().map(|k| k.vote_keypair.pubkey()),
            0,
        );

        // Advance some num slots; usually to the next epoch boundary to update
        // EpochStakes
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            advance_num_slots,
        );

        (
            RewardBank {
                bank,
                voters: validator_keypairs
                    .iter()
                    .map(|k| k.vote_keypair.pubkey())
                    .collect(),
                stakers: validator_keypairs
                    .iter()
                    .map(|k| k.stake_keypair.pubkey())
                    .collect(),
            },
            bank_forks,
        )
    }

    pub(super) fn populate_vote_accounts_with_votes(
        bank: &Bank,
        vote_pubkeys: impl IntoIterator<Item = Pubkey>,
        commission: u8,
    ) {
        for vote_pubkey in vote_pubkeys {
            let mut vote_account = bank
                .get_account(&vote_pubkey)
                .unwrap_or_else(|| panic!("missing vote account {vote_pubkey:?}"));
            let mut vote_state =
                Some(VoteStateV4::deserialize(vote_account.data(), &vote_pubkey).unwrap());
            if let Some(state) = vote_state.as_mut() {
                state.set_commission(commission);
            }
            for i in 0..MAX_LOCKOUT_HISTORY + 42 {
                if let Some(state) = vote_state.as_mut() {
                    vote_state::process_slot_vote_unchecked(state, i as u64);
                }
                let versioned = VoteStateVersions::V4(Box::new(vote_state.take().unwrap()));
                vote_account.set_state(&versioned).unwrap();
                match versioned {
                    VoteStateVersions::V4(v) => {
                        vote_state = Some(*v);
                    }
                    _ => panic!("Has to be of type V4"),
                };
            }
            bank.store_account_and_update_capitalization(&vote_pubkey, &vote_account);
        }
    }

    #[test]
    fn test_force_reward_interval_end() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let mut bank = Bank::new_for_tests(&genesis_config);

        let expected_num = 100;

        let stake_rewards = (0..expected_num)
            .map(|_| Some(PartitionedStakeReward::new_random()))
            .collect::<PartitionedStakeRewards>();

        let partition_indices = vec![(0..expected_num).collect()];

        bank.set_epoch_reward_status_distribution(
            bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            Arc::new(stake_rewards),
            partition_indices,
        );
        assert!(bank.get_reward_interval() == RewardInterval::InsideInterval);

        bank.force_reward_interval_end_for_tests();
        assert!(bank.get_reward_interval() == RewardInterval::OutsideInterval);
    }

    /// Test get_reward_distribution_num_blocks during small epoch
    /// The num_credit_blocks should be cap to 10% of the total number of blocks in the epoch.
    #[test]
    fn test_get_reward_distribution_num_blocks_cap() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        genesis_config.epoch_schedule = EpochSchedule::custom(32, 32, false);

        // Config stake reward distribution to be 10 per block
        let mut accounts_db_config: AccountsDbConfig = ACCOUNTS_DB_CONFIG_FOR_TESTING.clone();
        accounts_db_config.partitioned_epoch_rewards_config =
            PartitionedEpochRewardsConfig::new_for_test(10);

        let bank = Bank::new_from_genesis(
            &genesis_config,
            Arc::new(RuntimeConfig::default()),
            Vec::new(),
            None,
            accounts_db_config,
            None,
            Some(Pubkey::new_unique()),
            Arc::default(),
            None,
            None,
        );

        let stake_account_stores_per_block =
            bank.partitioned_rewards_stake_account_stores_per_block();
        assert_eq!(stake_account_stores_per_block, 10);

        let check_num_reward_distribution_blocks =
            |num_stakes: u64, expected_num_reward_distribution_blocks: u64| {
                // Given the short epoch, i.e. 32 slots, we should cap the number of reward distribution blocks to 32/10 = 3.
                let stake_rewards = (0..num_stakes)
                    .map(|_| Some(PartitionedStakeReward::new_random()))
                    .collect::<PartitionedStakeRewards>();

                assert_eq!(
                    bank.get_reward_distribution_num_blocks(&stake_rewards),
                    expected_num_reward_distribution_blocks
                );
            };

        for test_record in [
            // num_stakes, expected_num_reward_distribution_blocks
            (0, 1),
            (1, 1),
            (stake_account_stores_per_block, 1),
            (2 * stake_account_stores_per_block - 1, 2),
            (2 * stake_account_stores_per_block, 2),
            (3 * stake_account_stores_per_block - 1, 3),
            (3 * stake_account_stores_per_block, 3),
            (4 * stake_account_stores_per_block, 3), // cap at 3
            (5 * stake_account_stores_per_block, 3), //cap at 3
        ] {
            check_num_reward_distribution_blocks(test_record.0, test_record.1);
        }
    }

    /// Test get_reward_distribution_num_blocks during normal epoch gives the expected result
    #[test]
    fn test_get_reward_distribution_num_blocks_normal() {
        agave_logger::setup();
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        genesis_config.epoch_schedule = EpochSchedule::custom(432000, 432000, false);

        let bank = Bank::new_for_tests(&genesis_config);

        // Given 8k rewards, it will take 2 blocks to credit all the rewards
        let expected_num = 8192;
        let stake_rewards = (0..expected_num)
            .map(|_| Some(PartitionedStakeReward::new_random()))
            .collect::<PartitionedStakeRewards>();

        assert_eq!(bank.get_reward_distribution_num_blocks(&stake_rewards), 2);
    }

    /// Test get_reward_distribution_num_blocks during warm up epoch gives the expected result.
    /// The num_credit_blocks should be 1 during warm up epoch.
    #[test]
    fn test_get_reward_distribution_num_blocks_warmup() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);

        let bank = Bank::new_for_tests(&genesis_config);
        let rewards = PartitionedStakeRewards::default();
        assert_eq!(bank.get_reward_distribution_num_blocks(&rewards), 1);
    }

    /// Test get_reward_distribution_num_blocks with `None` elements in the
    /// partitioned stake rewards. `None` elements can occur if for any stake
    /// delegation:
    /// * there is no payout or if any deserved payout is < 1 lamport
    /// * corresponding vote account was not found in cache and accounts-db
    #[test]
    fn test_get_reward_distribution_num_blocks_none() {
        let rewards_all = 8192;
        let expected_rewards_some = 6144;
        let rewards = (0..rewards_all)
            .map(|i| {
                if i % 4 == 0 {
                    None
                } else {
                    Some(PartitionedStakeReward::new_random())
                }
            })
            .collect::<PartitionedStakeRewards>();
        assert_eq!(rewards.rewards.len(), rewards_all);
        assert_eq!(rewards.num_rewards(), expected_rewards_some);

        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);
        assert_eq!(bank.get_reward_distribution_num_blocks(&rewards), 1);
    }

    #[test]
    fn test_rewards_computation_and_partitioned_distribution_one_block() {
        agave_logger::setup();

        let starting_slot = SLOTS_PER_EPOCH - 1;
        let (
            RewardBank {
                bank: mut previous_bank,
                ..
            },
            bank_forks,
        ) = create_default_reward_bank(100, starting_slot - 1);

        // simulate block progress
        for slot in starting_slot..=(2 * SLOTS_PER_EPOCH) + 2 {
            let pre_cap = previous_bank.capitalization();
            let curr_bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                previous_bank.clone(),
                &Pubkey::default(),
                slot,
            );
            let post_cap = curr_bank.capitalization();

            if slot % SLOTS_PER_EPOCH == 0 {
                // This is the first block of the epoch. Reward computation should happen in this block.
                // assert reward compute status activated at epoch boundary
                assert_matches!(
                    curr_bank.get_reward_interval(),
                    RewardInterval::InsideInterval
                );

                assert!(curr_bank.is_calculated());

                // after reward calculation, the cache should be filled.
                assert!(curr_bank
                    .get_epoch_rewards_from_cache(&curr_bank.parent_hash)
                    .is_some());
                assert_eq!(post_cap, pre_cap);

                // Make a root the bank, which is the first bank in the epoch.
                // This will clear the cache.
                let _ = bank_forks.write().unwrap().set_root(slot, None, None);
                assert_eq!(curr_bank.get_epoch_rewards_cache_len(), 0);
            } else if slot == SLOTS_PER_EPOCH + 1 {
                // 1. when curr_slot == SLOTS_PER_EPOCH + 1, the 2nd block of
                // epoch 1, reward distribution should happen in this block.
                // however, all stake rewards are paid at this block therefore
                // reward_status should have transitioned to inactive. The cap
                // should increase accordingly.
                assert_matches!(
                    curr_bank.get_reward_interval(),
                    RewardInterval::OutsideInterval
                );
                let account = curr_bank
                    .get_account(&solana_sysvar::epoch_rewards::id())
                    .unwrap();
                let epoch_rewards: solana_sysvar::epoch_rewards::EpochRewards =
                    solana_account::from_account(&account).unwrap();
                assert_eq!(post_cap, pre_cap + epoch_rewards.distributed_rewards);
            } else {
                // 2. when curr_slot == SLOTS_PER_EPOCH + 2, the 3rd block of
                // epoch 1 (or any other slot). reward distribution should have
                // already completed. Therefore, reward_status should stay
                // inactive and cap should stay the same.
                assert_matches!(
                    curr_bank.get_reward_interval(),
                    RewardInterval::OutsideInterval
                );

                // slot is not in rewards, cap should not change
                assert_eq!(post_cap, pre_cap);
            }
            // EpochRewards sysvar is created in the first block of epoch 1.
            // Ensure the sysvar persists thereafter.
            if slot >= SLOTS_PER_EPOCH {
                let epoch_rewards_lamports =
                    curr_bank.get_balance(&solana_sysvar::epoch_rewards::id());
                assert!(epoch_rewards_lamports > 0);
            }
            previous_bank = curr_bank;
        }
    }

    /// Test rewards computation and partitioned rewards distribution at the epoch boundary (two reward distribution blocks)
    #[test]
    fn test_rewards_computation_and_partitioned_distribution_two_blocks() {
        agave_logger::setup();

        let starting_slot = SLOTS_PER_EPOCH - 1;
        let (
            RewardBank {
                bank: mut previous_bank,
                ..
            },
            bank_forks,
        ) = create_reward_bank(100, 50, starting_slot - 1);
        let mut starting_hash = None;

        // simulate block progress
        for slot in starting_slot..=SLOTS_PER_EPOCH + 3 {
            let pre_cap = previous_bank.capitalization();

            let pre_sysvar_account = previous_bank
                .get_account(&solana_sysvar::epoch_rewards::id())
                .unwrap_or_default();
            let pre_epoch_rewards: solana_sysvar::epoch_rewards::EpochRewards =
                solana_account::from_account(&pre_sysvar_account).unwrap_or_default();
            let pre_distributed_rewards = pre_epoch_rewards.distributed_rewards;
            let curr_bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                previous_bank.clone(),
                &Pubkey::default(),
                slot,
            );
            let post_cap = curr_bank.capitalization();

            if slot == SLOTS_PER_EPOCH {
                // This is the first block of epoch 1. Reward computation should happen in this block.
                // assert reward compute status activated at epoch boundary
                assert_matches!(
                    curr_bank.get_reward_interval(),
                    RewardInterval::InsideInterval
                );

                // calculation block, state should be calculated.
                assert!(curr_bank.is_calculated());

                // after reward calculation, the cache should be filled.
                assert!(curr_bank
                    .get_epoch_rewards_from_cache(&curr_bank.parent_hash)
                    .is_some());
                assert_eq!(curr_bank.get_epoch_rewards_cache_len(), 1);
                starting_hash = Some(curr_bank.parent_hash);
            } else if slot == SLOTS_PER_EPOCH + 1 {
                // When curr_slot == SLOTS_PER_EPOCH + 1, the 2nd block of
                // epoch 1, reward distribution should happen in this block. The
                // cap should increase accordingly.
                assert_matches!(
                    curr_bank.get_reward_interval(),
                    RewardInterval::InsideInterval
                );

                // The first block of the epoch has not rooted yet, so the cache
                // should still have the results.
                assert!(curr_bank
                    .get_epoch_rewards_from_cache(&starting_hash.unwrap())
                    .is_some());
                assert_eq!(curr_bank.get_epoch_rewards_cache_len(), 1);

                // 1st reward distribution block, state should be partitioned.
                assert!(curr_bank.is_partitioned());

                let account = curr_bank
                    .get_account(&solana_sysvar::epoch_rewards::id())
                    .unwrap();
                let epoch_rewards: solana_sysvar::epoch_rewards::EpochRewards =
                    solana_account::from_account(&account).unwrap();
                assert_eq!(
                    post_cap,
                    pre_cap + epoch_rewards.distributed_rewards - pre_distributed_rewards
                );

                // Now make a root the  first bank in the epoch.
                // This should clear the cache.
                let _ = bank_forks.write().unwrap().set_root(slot - 1, None, None);
                assert_eq!(curr_bank.get_epoch_rewards_cache_len(), 0);
            } else if slot == SLOTS_PER_EPOCH + 2 {
                // When curr_slot == SLOTS_PER_EPOCH + 2, the 3rd block of
                // epoch 1, reward distribution should happen in this block.
                // however, all stake rewards are paid at the this block
                // therefore reward_status should have transitioned to inactive.
                // The cap should increase accordingly.
                assert_matches!(
                    curr_bank.get_reward_interval(),
                    RewardInterval::OutsideInterval
                );

                let account = curr_bank
                    .get_account(&solana_sysvar::epoch_rewards::id())
                    .unwrap();
                let epoch_rewards: solana_sysvar::epoch_rewards::EpochRewards =
                    solana_account::from_account(&account).unwrap();
                assert_eq!(
                    post_cap,
                    pre_cap + epoch_rewards.distributed_rewards - pre_distributed_rewards
                );
            } else {
                // When curr_slot == SLOTS_PER_EPOCH + 3, the 4th block of
                // epoch 1 (or any other slot). reward distribution should have
                // already completed. Therefore, reward_status should stay
                // inactive and cap should stay the same.
                assert_matches!(
                    curr_bank.get_reward_interval(),
                    RewardInterval::OutsideInterval
                );

                // slot is not in rewards, cap should not change
                assert_eq!(post_cap, pre_cap);
            }
            previous_bank = curr_bank;
        }
    }

    /// Test that lamports can be sent to stake accounts regardless of rewards period.
    #[test]
    fn test_rewards_period_system_transfer() {
        let validator_vote_keypairs = ValidatorVoteKeypairs::new_rand();
        let validator_keypairs = vec![&validator_vote_keypairs];
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![1_000_000_000; 1],
        );

        // Add stake account to try to mutate
        let vote_key = validator_keypairs[0].vote_keypair.pubkey();
        let vote_account = genesis_config
            .accounts
            .iter()
            .find(|(address, _)| **address == vote_key)
            .map(|(_, account)| account)
            .unwrap()
            .clone();

        let new_stake_signer = Keypair::new();
        let new_stake_address = new_stake_signer.pubkey();
        let new_stake_account = Account::from(stake_utils::create_stake_account(
            &new_stake_address,
            &vote_key,
            &vote_account.into(),
            &genesis_config.rent,
            2_000_000_000,
        ));
        genesis_config
            .accounts
            .extend(vec![(new_stake_address, new_stake_account)]);

        let (mut previous_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let num_slots_in_epoch = previous_bank.get_slots_in_epoch(previous_bank.epoch());
        assert_eq!(num_slots_in_epoch, 32);

        let transfer_amount = 5_000;

        for slot in 1..=num_slots_in_epoch + 2 {
            let bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                previous_bank.clone(),
                &Pubkey::default(),
                slot,
            );

            // Fill bank_forks with banks with votes landing in the next slot
            // So that rewards will be paid out at the epoch boundary, i.e. slot = 32
            let tower_sync = TowerSync::new_from_slot(slot - 1, previous_bank.hash());
            let vote = vote_transaction::new_tower_sync_transaction(
                tower_sync,
                previous_bank.last_blockhash(),
                &validator_vote_keypairs.node_keypair,
                &validator_vote_keypairs.vote_keypair,
                &validator_vote_keypairs.vote_keypair,
                None,
            );
            bank.process_transaction(&vote).unwrap();

            // Insert a transfer transaction from the mint to new stake account
            let system_tx = system_transaction::transfer(
                &mint_keypair,
                &new_stake_address,
                transfer_amount,
                bank.last_blockhash(),
            );
            let system_result = bank.process_transaction(&system_tx);

            // Credits should always succeed
            assert!(system_result.is_ok());

            // Push a dummy blockhash, so that the latest_blockhash() for the transfer transaction in each
            // iteration are different. Otherwise, all those transactions will be the same, and will not be
            // executed by the bank except the first one.
            bank.register_unique_recent_blockhash_for_test();
            previous_bank = bank;
        }
    }

    #[test]
    fn test_get_rewards_and_partitions() {
        let starting_slot = SLOTS_PER_EPOCH - 1;
        let num_rewards = 100;
        let stake_account_stores_per_block = 50;
        let (RewardBank { bank, .. }, _) =
            create_reward_bank(num_rewards, stake_account_stores_per_block, starting_slot);

        // Slot before the epoch boundary contains empty rewards (since fees are
        // off), and no partitions because not at the epoch boundary
        assert_eq!(
            bank.get_rewards_and_num_partitions(),
            KeyedRewardsAndNumPartitions {
                keyed_rewards: vec![],
                num_partitions: None,
            }
        );

        let epoch_boundary_bank = Arc::new(Bank::new_from_parent(
            bank,
            &Pubkey::default(),
            SLOTS_PER_EPOCH,
        ));
        // Slot at the epoch boundary contains voting rewards only, as well as partition data
        let KeyedRewardsAndNumPartitions {
            keyed_rewards,
            num_partitions,
        } = epoch_boundary_bank.get_rewards_and_num_partitions();
        for (_pubkey, reward) in keyed_rewards.iter() {
            assert_eq!(reward.reward_type, RewardType::Voting);
        }
        assert_eq!(keyed_rewards.len(), num_rewards);
        assert_eq!(
            num_partitions,
            Some(num_rewards as u64 / stake_account_stores_per_block)
        );

        let mut total_staking_rewards = 0;

        let partition0_bank = Arc::new(Bank::new_from_parent(
            epoch_boundary_bank,
            &Pubkey::default(),
            SLOTS_PER_EPOCH + 1,
        ));
        // Slot after the epoch boundary contains first partition of staking
        // rewards, and no partitions because not at the epoch boundary
        let KeyedRewardsAndNumPartitions {
            keyed_rewards,
            num_partitions,
        } = partition0_bank.get_rewards_and_num_partitions();
        for (_pubkey, reward) in keyed_rewards.iter() {
            assert_eq!(reward.reward_type, RewardType::Staking);
        }
        total_staking_rewards += keyed_rewards.len();
        assert_eq!(num_partitions, None);

        let partition1_bank = Arc::new(Bank::new_from_parent(
            partition0_bank,
            &Pubkey::default(),
            SLOTS_PER_EPOCH + 2,
        ));
        // Slot 2 after the epoch boundary contains second partition of staking
        // rewards, and no partitions because not at the epoch boundary
        let KeyedRewardsAndNumPartitions {
            keyed_rewards,
            num_partitions,
        } = partition1_bank.get_rewards_and_num_partitions();
        for (_pubkey, reward) in keyed_rewards.iter() {
            assert_eq!(reward.reward_type, RewardType::Staking);
        }
        total_staking_rewards += keyed_rewards.len();
        assert_eq!(num_partitions, None);

        // All rewards are recorded
        assert_eq!(total_staking_rewards, num_rewards);

        let bank = Bank::new_from_parent(partition1_bank, &Pubkey::default(), SLOTS_PER_EPOCH + 3);
        // Next slot contains empty rewards (since fees are off), and no
        // partitions because not at the epoch boundary
        assert_eq!(
            bank.get_rewards_and_num_partitions(),
            KeyedRewardsAndNumPartitions {
                keyed_rewards: vec![],
                num_partitions: None,
            }
        );
    }

    #[test]
    fn test_rewards_and_partitions_should_record() {
        let reward = RewardInfo {
            reward_type: RewardType::Voting,
            lamports: 55,
            post_balance: 5555,
            commission: Some(5),
        };

        let rewards_and_partitions = KeyedRewardsAndNumPartitions {
            keyed_rewards: vec![],
            num_partitions: None,
        };
        assert!(!rewards_and_partitions.should_record());

        let rewards_and_partitions = KeyedRewardsAndNumPartitions {
            keyed_rewards: vec![(Pubkey::new_unique(), reward)],
            num_partitions: None,
        };
        assert!(rewards_and_partitions.should_record());

        let rewards_and_partitions = KeyedRewardsAndNumPartitions {
            keyed_rewards: vec![],
            num_partitions: Some(42),
        };
        assert!(rewards_and_partitions.should_record());

        let rewards_and_partitions = KeyedRewardsAndNumPartitions {
            keyed_rewards: vec![(Pubkey::new_unique(), reward)],
            num_partitions: Some(42),
        };
        assert!(rewards_and_partitions.should_record());
    }
}
