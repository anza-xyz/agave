use {
    super::Bank,
    crate::inflation_rewards::points::PointValue,
    log::info,
    solana_account::{
        ReadableAccount, WritableAccount, create_account_shared_data_with_fields as create_account,
        from_account,
    },
    solana_clock::INITIAL_RENT_EPOCH,
    solana_sysvar::{self as sysvar, epoch_rewards::EpochRewards},
};

impl Bank {
    /// Helper fn to log epoch_rewards sysvar
    fn log_epoch_rewards_sysvar(&self, prefix: &str) {
        if let Some(account) = self.get_account(&sysvar::epoch_rewards::id()) {
            let epoch_rewards: EpochRewards = from_account(&account).unwrap();
            info!("{prefix} epoch_rewards sysvar: {epoch_rewards:?}");
        } else {
            info!("{prefix} epoch_rewards sysvar: none");
        }
    }

    /// Create EpochRewards sysvar with calculated rewards
    /// This method must be called before a new Bank advances its
    /// last_blockhash.
    pub(in crate::bank) fn create_epoch_rewards_sysvar(
        &self,
        distributed_rewards: u64,
        distribution_starting_block_height: u64,
        num_partitions: u64,
        point_value: &PointValue,
        block_rewards: u64,
    ) {
        assert!(point_value.rewards >= distributed_rewards);

        let parent_blockhash = self.last_blockhash();

        let epoch_rewards = EpochRewards {
            distribution_starting_block_height,
            num_partitions,
            parent_blockhash,
            total_points: point_value.points,
            total_rewards: point_value.rewards,
            distributed_rewards,
            active: true,
        };

        // Do the first store to create the account from scratch, update
        // capitalization if needed, etc
        self.update_sysvar_account(&sysvar::epoch_rewards::id(), |account| {
            create_account(
                &epoch_rewards,
                self.inherit_specially_retained_account_fields(account),
            )
        });

        // Now add the lamports separately without updating capitalization,
        // since block reward lamports already existed
        let mut account = self
            .get_account_with_fixed_root(&sysvar::epoch_rewards::id())
            .expect("created sysvar account exists");

        // SAFETY: block rewards come from existing lamports, which cannot
        // overflow
        account
            .checked_add_lamports(block_rewards)
            .expect("block rewards and sysvar account rent exemption must fit in a u64");
        self.store_account(&sysvar::epoch_rewards::id(), &account);

        self.log_epoch_rewards_sysvar("create");
    }

    /// Update EpochRewards sysvar with distributed rewards
    pub(in crate::bank::partitioned_epoch_rewards) fn update_epoch_rewards_sysvar(
        &self,
        distributed: u64,
        debit_block_reward_lamports: u64,
    ) {
        let mut epoch_rewards = self.get_epoch_rewards_sysvar();
        assert!(epoch_rewards.active);

        epoch_rewards.distribute(distributed);

        self.update_sysvar_account(&sysvar::epoch_rewards::id(), |account| {
            create_account(
                &epoch_rewards,
                self.inherit_specially_retained_account_fields(account),
            )
        });

        // Debit the lamports separately without updating capitalization,
        // since block reward lamports already existed
        let mut account = self
            .get_account_with_fixed_root(&sysvar::epoch_rewards::id())
            .expect("created sysvar account exists");

        // SAFETY: programmer error if we debit too many block rewards
        account
            .checked_sub_lamports(debit_block_reward_lamports)
            .expect("epoch reward sysvar has enough lamports for distribution");
        assert!(
            account.lamports() >= self.get_minimum_balance_for_rent_exemption(account.data().len()),
            "Sysvar account must have enough for rent exemption after debiting block rewards"
        );
        self.store_account(&sysvar::epoch_rewards::id(), &account);

        self.log_epoch_rewards_sysvar("update");
    }

    /// Update EpochRewards sysvar with distributed rewards and burn any
    /// remaining lamports over the rent-exempt reserve
    pub(in crate::bank::partitioned_epoch_rewards) fn set_epoch_rewards_sysvar_to_inactive(&self) {
        const RENT_UNADJUSTED_INITIAL_BALANCE: u64 = 1;

        let mut epoch_rewards = self.get_epoch_rewards_sysvar();
        assert!(epoch_rewards.total_rewards >= epoch_rewards.distributed_rewards);
        epoch_rewards.active = false;

        self.update_sysvar_account(&sysvar::epoch_rewards::id(), |account| {
            // Don't use `inherit_specially_retained_account_fields()` to
            // ensure that any remaining lamports get burned, lamports are
            // set to the rent-exempt minimum during `update_sysvar_account`,
            // and capitalization is updated
            create_account(
                &epoch_rewards,
                (
                    RENT_UNADJUSTED_INITIAL_BALANCE,
                    account
                        .as_ref()
                        .map(|a| a.rent_epoch())
                        .unwrap_or(INITIAL_RENT_EPOCH),
                ),
            )
        });

        self.log_epoch_rewards_sysvar("set_inactive");
    }

    /// Get EpochRewards sysvar. Returns EpochRewards::default() if sysvar
    /// account cannot be found or cannot be deserialized.
    pub(in crate::bank::partitioned_epoch_rewards) fn get_epoch_rewards_sysvar(
        &self,
    ) -> EpochRewards {
        from_account(
            &self
                .get_account(&sysvar::epoch_rewards::id())
                .unwrap_or_default(),
        )
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bank::{SlotLeader, tests::create_genesis_config},
        solana_epoch_schedule::EpochSchedule,
        solana_native_token::LAMPORTS_PER_SOL,
    };

    /// Test `EpochRewards` sysvar creation, distribution, and burning.
    /// This test covers the following epoch_rewards_sysvar bank member functions, i.e.
    /// `create_epoch_rewards_sysvar`, `update_epoch_rewards_sysvar`, `test_epoch_rewards_sysvar`.
    #[test]
    fn test_epoch_rewards_sysvar() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        genesis_config.epoch_schedule = EpochSchedule::custom(432000, 432000, false);
        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();

        let total_rewards = 1_000_000_000;
        let num_partitions = 2; // num_partitions is arbitrary and unimportant for this test
        let total_points = (total_rewards * 42) as u128; // total_points is arbitrary for the purposes of this test
        let point_value = PointValue {
            rewards: total_rewards,
            points: total_points,
        };

        let first_block_rewards = 5_000_000_000;

        // create epoch rewards sysvar
        let expected_epoch_rewards = EpochRewards {
            distribution_starting_block_height: 42,
            num_partitions,
            parent_blockhash: bank.last_blockhash(),
            total_points,
            total_rewards,
            distributed_rewards: 10,
            active: true,
        };

        let epoch_rewards = bank.get_epoch_rewards_sysvar();
        assert_eq!(epoch_rewards, EpochRewards::default());

        let pre_capitalization = bank.capitalization();
        bank.create_epoch_rewards_sysvar(10, 42, num_partitions, &point_value, first_block_rewards);
        let post_capitalization = bank.capitalization();

        let account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();
        let rent_exempt_reserve = bank.get_minimum_balance_for_rent_exemption(account.data().len());
        let expected_balance = rent_exempt_reserve + first_block_rewards;
        // Expected balance is the sysvar rent-exempt balance plus block rewards
        assert_eq!(account.lamports(), expected_balance);

        // Expect capitalization to only change by rent exempt minimum
        assert_eq!(
            post_capitalization,
            pre_capitalization + rent_exempt_reserve
        );

        let epoch_rewards: EpochRewards = from_account(&account).unwrap();
        assert_eq!(epoch_rewards, expected_epoch_rewards);

        // Unsetting should burn all block rewards
        bank.set_epoch_rewards_sysvar_to_inactive();
        assert_eq!(
            post_capitalization - first_block_rewards,
            bank.capitalization()
        );

        // Create a child bank to test parent_blockhash
        let parent_blockhash = bank.last_blockhash();
        let parent_slot = bank.slot();
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            parent_slot + 1,
        );
        let second_block_rewards = 500_000_000;

        // Also note that running `create_epoch_rewards_sysvar()` against a bank
        // with an existing EpochRewards sysvar clobbers the previous values
        let pre_capitalization = bank.capitalization();
        bank.create_epoch_rewards_sysvar(
            10,
            42,
            num_partitions,
            &point_value,
            second_block_rewards,
        );
        let post_capitalization = bank.capitalization();

        // Capitalization shouldn't change this time, no new lamports minted
        // since account already existed, but different amount of lamports on
        // account
        assert_eq!(post_capitalization, pre_capitalization);

        let expected_epoch_rewards = EpochRewards {
            distribution_starting_block_height: 42,
            num_partitions,
            parent_blockhash,
            total_points,
            total_rewards,
            distributed_rewards: 10,
            active: true,
        };

        let account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();
        let expected_balance = bank.get_minimum_balance_for_rent_exemption(account.data().len())
            + second_block_rewards;
        // Expected balance is the sysvar rent-exempt balance with new block rewards
        assert_eq!(account.lamports(), expected_balance);

        let epoch_rewards = bank.get_epoch_rewards_sysvar();
        assert_eq!(epoch_rewards, expected_epoch_rewards);

        // make a distribution from epoch rewards sysvar
        let block_reward_distribution = 1_000_000;
        bank.update_epoch_rewards_sysvar(10, block_reward_distribution);
        let account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();

        // Balance should change
        assert_eq!(
            account.lamports(),
            expected_balance - block_reward_distribution
        );

        // Capitalization should not
        assert_eq!(post_capitalization, bank.capitalization());

        let epoch_rewards: EpochRewards = from_account(&account).unwrap();
        let expected_epoch_rewards = EpochRewards {
            distribution_starting_block_height: 42,
            num_partitions,
            parent_blockhash,
            total_points,
            total_rewards,
            distributed_rewards: 20,
            active: true,
        };
        assert_eq!(epoch_rewards, expected_epoch_rewards);

        // Unsetting should burn the rest
        bank.set_epoch_rewards_sysvar_to_inactive();
        assert_eq!(
            bank.capitalization(),
            post_capitalization + block_reward_distribution - second_block_rewards
        );

        let account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();
        let epoch_rewards: EpochRewards = from_account(&account).unwrap();
        let expected_epoch_rewards = EpochRewards {
            distribution_starting_block_height: 42,
            num_partitions,
            parent_blockhash,
            total_points,
            total_rewards,
            distributed_rewards: 20,
            active: false,
        };
        assert_eq!(epoch_rewards, expected_epoch_rewards);
        assert_eq!(account.lamports(), rent_exempt_reserve);
    }
}
