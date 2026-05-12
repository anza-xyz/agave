use {
    super::Bank,
    crate::{bank::CollectorFeeDetails, reward_info::RewardInfo},
    agave_reserved_account_keys::ReservedAccountKeys,
    log::debug,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_fee::FeeFeatures,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_reward_info::RewardType,
    solana_runtime_transaction::{
        transaction_meta::TransactionConfiguration, transaction_with_meta::TransactionWithMeta,
    },
    solana_sdk_ids::incinerator,
    solana_svm::rent_calculator::check_static_account_rent_state_transition,
    solana_system_interface::program as system_program,
    std::{result::Result, sync::atomic::Ordering::Relaxed},
    thiserror::Error,
};

#[derive(Error, Debug, PartialEq)]
enum DepositFeeError {
    #[error("fee account became rent paying")]
    InvalidRentPayingAccount,
    #[error("lamport overflow")]
    LamportOverflow,
    #[error("invalid fee account owner")]
    InvalidAccountOwner,
    #[error("collector is a reserved account")]
    ReservedCollector,
}

#[derive(Default)]
pub struct FeeDistribution {
    deposit: u64,
    burn: u64,
}

impl FeeDistribution {
    pub fn get_deposit(&self) -> u64 {
        self.deposit
    }
}

impl Bank {
    // Distribute collected transaction fees for this slot to the block revenue collector
    // id for the current leader.
    //
    // Each validator is incentivized to process more transactions to earn more transaction fees.
    // Transaction fees are rewarded for the computing resource utilization cost, directly
    // proportional to their actual processing power.
    //
    // The leader is rotated according to stake-weighted leader schedule. So the opportunity of
    // earning transaction fees are fairly distributed by stake. And missing the opportunity
    // (not producing a block as a leader) earns nothing. So, being online is incentivized as a
    // form of transaction fees as well.
    pub(super) fn distribute_transaction_fee_details(&self) {
        let fee_details = self.collector_fee_details.read().unwrap();
        if fee_details.total_transaction_fee() == 0 {
            // nothing to distribute, exit early
            return;
        }

        let FeeDistribution { deposit, burn } =
            self.calculate_reward_and_burn_fee_details(&fee_details);

        let total_burn = self.deposit_or_burn_fee(deposit).saturating_add(burn);
        self.capitalization.fetch_sub(total_burn, Relaxed);
    }

    pub fn calculate_reward_for_transaction(
        &self,
        transaction: &impl TransactionWithMeta,
        transaction_configuration: &TransactionConfiguration,
    ) -> u64 {
        let fee_details = solana_fee::calculate_fee_details(
            transaction,
            self.fee_structure().lamports_per_signature,
            transaction_configuration.priority_fee_lamports,
            FeeFeatures::from(self.feature_set.as_ref()),
        );
        let FeeDistribution {
            deposit: reward,
            burn: _,
        } = self.calculate_reward_and_burn_fee_details(&CollectorFeeDetails::from(fee_details));
        reward
    }

    pub fn calculate_reward_and_burn_fee_details(
        &self,
        fee_details: &CollectorFeeDetails,
    ) -> FeeDistribution {
        if fee_details.transaction_fee == 0 {
            return FeeDistribution::default();
        }

        let burn = fee_details.transaction_fee * self.burn_percent() / 100;
        let deposit = fee_details
            .priority_fee
            .saturating_add(fee_details.transaction_fee.saturating_sub(burn));
        FeeDistribution { deposit, burn }
    }

    const fn burn_percent(&self) -> u64 {
        // NOTE: burn percent is statically 50%, in case it needs to change in the future,
        // burn_percent can be bank property that being passed down from bank to bank, without
        // needing fee-rate-governor
        static_assertions::const_assert!(solana_fee_calculator::DEFAULT_BURN_PERCENT <= 100);

        solana_fee_calculator::DEFAULT_BURN_PERCENT as u64
    }

    /// Attempts to deposit the given `deposit` amount into the fee collector account.
    ///
    /// Returns the original `deposit` amount if the deposit failed and must be burned, otherwise 0.
    fn deposit_or_burn_fee(&self, deposit: u64) -> u64 {
        if deposit == 0 {
            return 0;
        }

        // Per SIMD-0232: the commission collector address should be fetched
        // from the state of the vote account at the beginning of the previous
        // epoch. This is the vote account state used to build the leader
        // schedule for the current epoch, which *DOES NOT* correspond to
        // `Bank::current_epoch_stakes()`.
        let feature_snapshot = self.feature_set.snapshot();
        let collector_id = if feature_snapshot.custom_commission_collector {
            let vote_account = self
                .epoch_stakes
                .get(&self.epoch)
                .and_then(|stakes| {
                    stakes
                        .stakes()
                        .vote_accounts()
                        .get(&self.leader.vote_address)
                })
                .expect("The vote account for the leader must exist");
            // Protection in case the leader is on a vote state without a
            // collector id, which can happen if a dormant pre-v4 vote state
            // accrues stake.
            vote_account
                .vote_state_view()
                .block_revenue_collector()
                .unwrap_or(&self.leader.id)
        } else {
            &self.leader.id
        };

        match self.deposit_fees(collector_id, deposit) {
            Ok(post_balance) => {
                self.rewards.write().unwrap().push((
                    *collector_id,
                    RewardInfo {
                        reward_type: RewardType::Fee,
                        lamports: deposit as i64,
                        post_balance,
                        commission_bps: None,
                    },
                ));
                0
            }
            Err(err) => {
                debug!(
                    "Burned {deposit} lamport tx fee instead of sending to {collector_id} due to \
                     {err}"
                );
                datapoint_warn!(
                    "bank-burned_fee",
                    ("slot", self.slot(), i64),
                    ("num_lamports", deposit, i64),
                    ("error", err.to_string(), String),
                );
                deposit
            }
        }
    }

    // Deposits fees into a specified account and if successful, returns the new balance of that account
    fn deposit_fees(&self, collector_id: &Pubkey, fees: u64) -> Result<u64, DepositFeeError> {
        let mut account = self
            .get_account_with_fixed_root_no_cache(collector_id)
            .unwrap_or_default();

        let feature_snapshot = self.feature_set.snapshot();
        if feature_snapshot.custom_commission_collector {
            let pre_lamports = account.lamports();
            account
                .checked_add_lamports(fees)
                .map_err(|_| DepositFeeError::LamportOverflow)?;
            if collector_id != &self.leader.vote_address {
                Bank::check_collector_account(
                    collector_id,
                    pre_lamports,
                    &account,
                    &self.reserved_account_keys,
                    &self.rent_collector().rent,
                    feature_snapshot.relax_post_exec_min_balance_check,
                )?;
            }
        } else {
            if !system_program::check_id(account.owner()) {
                return Err(DepositFeeError::InvalidAccountOwner);
            }

            let pre_balance = account.lamports();
            let distribution = account.checked_add_lamports(fees);
            if distribution.is_err() {
                return Err(DepositFeeError::LamportOverflow);
            }

            // rent state transition must be checked in case the account receiving the distribution
            // doesn't exist yet.
            if check_static_account_rent_state_transition(
                pre_balance,
                account.lamports(),
                account.data().len(),
                &self.rent_collector().rent,
                0, // account index isn't relevant and only used for error message
                feature_snapshot.relax_post_exec_min_balance_check,
            )
            .is_err()
            {
                return Err(DepositFeeError::InvalidRentPayingAccount);
            }
        }

        self.store_account(collector_id, &account);
        Ok(account.lamports())
    }

    fn check_collector_account(
        collector_id: &Pubkey,
        pre_lamports: u64,
        account: &AccountSharedData,
        reserved_account_keys: &ReservedAccountKeys,
        rent: &Rent,
        relax_post_execution_balance_checks: bool,
    ) -> Result<(), DepositFeeError> {
        if !system_program::check_id(account.owner()) {
            return Err(DepositFeeError::InvalidAccountOwner);
        }

        if reserved_account_keys.is_reserved(collector_id) {
            return Err(DepositFeeError::ReservedCollector);
        }

        // Don't perform rent check on the incinerator, so that the deposit
        // always works. The incinerator is cleaned right after this step
        if *collector_id != incinerator::id()
            && !rent.is_exempt(account.lamports(), account.data().len())
            && (!relax_post_execution_balance_checks || pre_lamports == 0)
        {
            return Err(DepositFeeError::InvalidRentPayingAccount);
        }

        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::genesis_utils::{create_genesis_config, create_genesis_config_with_leader},
        agave_feature_set::FeatureSet,
        solana_account::state_traits::StateMut,
        solana_pubkey as pubkey,
        solana_rent::Rent,
        solana_signer::Signer,
        solana_vote_interface::state::{VoteStateV4, VoteStateVersions},
        std::sync::{Arc, RwLock},
        test_case::test_case,
    };

    #[test]
    fn test_deposit_or_burn_zero_fee() {
        let genesis = create_genesis_config(0);
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        assert_eq!(bank.deposit_or_burn_fee(0), 0);
    }

    #[test_case(true; "custom_commission_collector")]
    #[test_case(false; "no_custom_commission_collector")]
    fn test_deposit_or_burn_fee(custom_commission_collector: bool) {
        #[derive(PartialEq)]
        enum Scenario {
            Normal,
            InvalidOwner,
            RentPayingAccount,
            NonDefault,
            VoteAccount,
            Incinerator,
        }

        struct TestCase {
            scenario: Scenario,
        }

        impl TestCase {
            fn new(scenario: Scenario) -> Self {
                Self { scenario }
            }
        }

        for test_case in [
            TestCase::new(Scenario::Normal),
            TestCase::new(Scenario::InvalidOwner),
            TestCase::new(Scenario::RentPayingAccount),
            TestCase::new(Scenario::NonDefault),
            TestCase::new(Scenario::VoteAccount),
            TestCase::new(Scenario::Incinerator),
        ] {
            if !custom_commission_collector {
                // Some scenarios don't make sense without a custom collector
                match test_case.scenario {
                    Scenario::NonDefault | Scenario::VoteAccount | Scenario::Incinerator => {
                        continue;
                    }
                    Scenario::Normal | Scenario::InvalidOwner | Scenario::RentPayingAccount => {}
                }
            }
            let initial_balance = 1000;
            let mut genesis =
                create_genesis_config_with_leader(0, &pubkey::new_rand(), initial_balance);
            let rent = Rent::default();
            let min_rent_exempt_balance = rent.minimum_balance(0);
            genesis.genesis_config.rent = rent; // Ensure rent is non-zero, as genesis_utils sets Rent::free by default

            // update collector id at genesis for some cases
            let maybe_collector_id = if custom_commission_collector {
                let mut maybe_collector_id = None;
                for (address, account) in genesis.genesis_config.accounts.iter_mut() {
                    if account.owner == solana_sdk_ids::vote::id() {
                        let mut vote_state =
                            VoteStateV4::deserialize(account.data(), &Pubkey::default()).unwrap();
                        let collector_id = match test_case.scenario {
                            Scenario::Normal => vote_state.block_revenue_collector,
                            Scenario::InvalidOwner
                            | Scenario::RentPayingAccount
                            | Scenario::NonDefault => Pubkey::new_unique(),
                            Scenario::Incinerator => incinerator::id(),
                            Scenario::VoteAccount => *address,
                        };
                        vote_state.block_revenue_collector = collector_id;
                        maybe_collector_id = Some(collector_id);
                        let versioned = VoteStateVersions::V4(Box::new(vote_state));
                        account.set_state(&versioned).unwrap();
                    }
                }
                maybe_collector_id
            } else {
                None
            };

            let mut bank = Bank::new_for_tests(&genesis.genesis_config);
            let mut feature_set = FeatureSet::all_enabled();
            if !custom_commission_collector {
                feature_set.deactivate(&agave_feature_set::custom_commission_collector::id());
            }
            bank.feature_set = Arc::new(feature_set);

            let collector_id = maybe_collector_id.unwrap_or(*bank.leader_id());

            let deposit = 100;
            let mut burn = 100;

            match test_case.scenario {
                Scenario::RentPayingAccount => {
                    // ensure that the account is rent-paying
                    let account = AccountSharedData::new(1, 1_000, &Pubkey::new_unique());
                    bank.store_account(&collector_id, &account);
                }
                Scenario::InvalidOwner => {
                    // ensure that account owner is invalid and fee distribution will fail
                    let account =
                        AccountSharedData::new(min_rent_exempt_balance, 0, &Pubkey::new_unique());
                    bank.store_account(&collector_id, &account);
                }
                Scenario::VoteAccount => {
                    // nothing to do, collector id already set, and vote account
                    // already exists
                }
                Scenario::Incinerator => {
                    // nothing to do, incinerator already exists
                }
                Scenario::NonDefault | Scenario::Normal => {
                    let account =
                        AccountSharedData::new(min_rent_exempt_balance, 0, &system_program::id());
                    bank.store_account(&collector_id, &account);
                }
            }

            let initial_burn = burn;
            let initial_collector_balance = bank.get_balance(&collector_id);
            burn += bank.deposit_or_burn_fee(deposit);
            let new_collector_balance = bank.get_balance(&collector_id);

            match test_case.scenario {
                Scenario::InvalidOwner | Scenario::RentPayingAccount => {
                    assert_eq!(initial_collector_balance, new_collector_balance);
                    assert_eq!(initial_burn + deposit, burn);
                    let locked_rewards = bank.rewards.read().unwrap();
                    assert!(
                        locked_rewards.is_empty(),
                        "There should be no rewards distributed"
                    );
                }
                Scenario::NonDefault
                | Scenario::Normal
                | Scenario::VoteAccount
                | Scenario::Incinerator => {
                    assert_eq!(initial_collector_balance + deposit, new_collector_balance);

                    assert_eq!(initial_burn, burn);

                    let locked_rewards = bank.rewards.read().unwrap();
                    assert_eq!(
                        locked_rewards.len(),
                        1,
                        "There should be one reward distributed"
                    );

                    let reward_info = &locked_rewards[0];
                    assert_eq!(
                        reward_info.1.lamports, deposit as i64,
                        "The reward amount should match the expected deposit"
                    );
                    assert_eq!(
                        reward_info.1.reward_type,
                        RewardType::Fee,
                        "The reward type should be Fee"
                    );
                }
            }
        }
    }

    #[test]
    fn test_deposit_fees() {
        let initial_balance = 1_000_000_000;
        let genesis = create_genesis_config(initial_balance);
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        let pubkey = genesis.mint_keypair.pubkey();
        let deposit_amount = 500;

        assert_eq!(
            bank.deposit_fees(&pubkey, deposit_amount),
            Ok(initial_balance + deposit_amount),
            "New balance should be the sum of the initial balance and deposit amount"
        );
    }

    #[test]
    fn test_deposit_fees_with_overflow() {
        let initial_balance = u64::MAX;
        let genesis = create_genesis_config(initial_balance);
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        let pubkey = genesis.mint_keypair.pubkey();
        let deposit_amount = 500;

        assert_eq!(
            bank.deposit_fees(&pubkey, deposit_amount),
            Err(DepositFeeError::LamportOverflow),
            "Expected an error due to lamport overflow"
        );
    }

    #[test_case(true, Ok(()); "allowed")]
    #[test_case(false, Err(DepositFeeError::InvalidAccountOwner); "prohibited")]
    fn test_deposit_fees_to_vote_account(
        custom_commission_collector: bool,
        expected: Result<(), DepositFeeError>,
    ) {
        let initial_balance = 1000;
        let genesis = create_genesis_config_with_leader(0, &pubkey::new_rand(), initial_balance);
        let mut bank = Bank::new_for_tests(&genesis.genesis_config);
        let mut feature_set = FeatureSet::all_enabled();
        if !custom_commission_collector {
            feature_set.deactivate(&agave_feature_set::custom_commission_collector::id());
        }
        bank.feature_set = Arc::new(feature_set);

        let pubkey = genesis.voting_keypair.pubkey();
        let deposit_amount = 500;
        let pre_lamports = bank.get_balance(&pubkey);
        assert_eq!(
            expected.map(|_| pre_lamports.saturating_add(deposit_amount)),
            bank.deposit_fees(&pubkey, deposit_amount)
        );
    }

    #[test]
    fn test_deposit_fees_reserved_account() {
        let initial_balance = 1000;
        let genesis = create_genesis_config_with_leader(0, &pubkey::new_rand(), initial_balance);
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        let deposit_amount = 500;

        for id in bank.get_reserved_account_keys() {
            assert!(matches!(
                bank.deposit_fees(id, deposit_amount),
                Err(DepositFeeError::ReservedCollector) | Err(DepositFeeError::InvalidAccountOwner),
            ));
        }
    }

    #[test]
    fn test_deposit_fees_to_nonexistent_account_rent_exempt() {
        let mut genesis = create_genesis_config(0);
        let rent = Rent::default();
        genesis.genesis_config.rent = rent.clone();
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        let nonexistent_pubkey = Pubkey::new_unique();

        // Fee is sufficient to make the new account rent-exempt
        let deposit_amount = rent.minimum_balance(0);

        assert!(
            bank.get_account(&nonexistent_pubkey).is_none(),
            "Account should not exist before deposit"
        );

        assert_eq!(
            bank.deposit_fees(&nonexistent_pubkey, deposit_amount),
            Ok(deposit_amount),
            "Deposit should succeed when fee is sufficient for rent-exemption"
        );

        let account = bank.get_account(&nonexistent_pubkey).unwrap();
        assert_eq!(account.lamports(), deposit_amount);
        assert_eq!(account.owner(), &system_program::id());
    }

    #[test]
    fn test_deposit_fees_to_nonexistent_account_not_rent_exempt() {
        let mut genesis = create_genesis_config(0);
        let rent = Rent::default();
        genesis.genesis_config.rent = rent.clone();
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        let nonexistent_pubkey = Pubkey::new_unique();

        // Fee is insufficient to make the new account rent-exempt
        let deposit_amount = rent.minimum_balance(0) - 1;

        assert!(
            bank.get_account(&nonexistent_pubkey).is_none(),
            "Account should not exist before deposit"
        );

        assert_eq!(
            bank.deposit_fees(&nonexistent_pubkey, deposit_amount),
            Err(DepositFeeError::InvalidRentPayingAccount),
            "Deposit should fail when fee is insufficient for rent-exemption"
        );

        assert!(
            bank.get_account(&nonexistent_pubkey).is_none(),
            "Account should still not exist after failed deposit"
        );
    }

    #[test_case(true; "custom_commission_collector")]
    #[test_case(false; "no_custom_commission_collector")]
    fn test_deposit_or_burn_fee_respects_relaxed_post_exec_min_balance_check(
        custom_commission_collector: bool,
    ) {
        enum CollectorState {
            InitializedToSubRentExemptMinimum,
            UninitializedToSubRentExemptMinimum,
            UninitializedToRentExempt,
        }

        for collector_state in [
            CollectorState::InitializedToSubRentExemptMinimum,
            CollectorState::UninitializedToSubRentExemptMinimum,
            CollectorState::UninitializedToRentExempt,
        ] {
            for relax_post_exec_min_balance_check in [false, true] {
                let mut genesis = create_genesis_config_with_leader(0, &pubkey::new_rand(), 1000);
                let rent = Rent::default();
                genesis.genesis_config.rent = rent.clone();

                let initialized_data_len = 64;
                let rent_exempt_minimum = rent.minimum_balance(initialized_data_len);
                assert!(rent_exempt_minimum > 1);

                let (pre_balance, deposit, should_succeed) = match collector_state {
                    CollectorState::InitializedToSubRentExemptMinimum => (
                        rent_exempt_minimum - 2,
                        1,
                        relax_post_exec_min_balance_check,
                    ),
                    CollectorState::UninitializedToSubRentExemptMinimum => (0, 1, false),
                    CollectorState::UninitializedToRentExempt => (0, rent_exempt_minimum, true),
                };
                let post_balance = pre_balance + deposit;
                let maybe_collector_id = if custom_commission_collector {
                    let mut maybe_collector_id = None;
                    for account in genesis.genesis_config.accounts.values_mut() {
                        if account.owner == solana_sdk_ids::vote::id() {
                            let mut vote_state =
                                VoteStateV4::deserialize(account.data(), &Pubkey::default())
                                    .unwrap();
                            let collector_id = Pubkey::new_unique();
                            vote_state.block_revenue_collector = collector_id;
                            maybe_collector_id = Some(collector_id);
                            let versioned = VoteStateVersions::V4(Box::new(vote_state));
                            account.set_state(&versioned).unwrap();
                        }
                    }
                    maybe_collector_id
                } else {
                    None
                };

                let mut bank = Bank::new_for_tests(&genesis.genesis_config);

                let mut feature_set = FeatureSet::all_enabled();
                if !custom_commission_collector {
                    feature_set.deactivate(&agave_feature_set::custom_commission_collector::id());
                }
                if !relax_post_exec_min_balance_check {
                    feature_set
                        .deactivate(&agave_feature_set::relax_post_exec_min_balance_check::id());
                }
                bank.feature_set = Arc::new(feature_set);

                let collector_id = maybe_collector_id.unwrap_or(*bank.leader_id());
                let account = AccountSharedData::new(
                    pre_balance,
                    initialized_data_len,
                    &system_program::id(),
                );
                bank.store_account(&collector_id, &account);

                let burned = bank.deposit_or_burn_fee(deposit);
                let rewards = bank.rewards.read().unwrap();

                // post simd-392, deposits to existing accounts are always valid because
                // they are rent-exempt before and after the deposit takes place.
                // Deposits to uninitialized accounts still must make the account rent-exempt.
                // pre simd-392, if a deposit to a rent-paying account isn't sufficient to
                // make it rent-exempt then it fails and the deposit is burned.
                if should_succeed {
                    assert_eq!(burned, 0);
                    assert_eq!(bank.get_balance(&collector_id), post_balance);
                    assert_eq!(rewards.len(), 1, "fee should be distributed to the leader");
                    assert_eq!(rewards[0].1.post_balance, post_balance);
                } else {
                    assert_eq!(burned, deposit);
                    assert_eq!(bank.get_balance(&collector_id), pre_balance);
                    assert!(
                        rewards.is_empty(),
                        "fee should be burned when the rent transition is invalid"
                    );
                }
            }
        }
    }

    #[test]
    fn test_distribute_transaction_fee_details_normal() {
        let initial_balance = 1000;
        let genesis = create_genesis_config_with_leader(0, &pubkey::new_rand(), initial_balance);
        let mut bank = Bank::new_for_tests(&genesis.genesis_config);
        let transaction_fee = 100;
        let priority_fee = 200;
        bank.collector_fee_details = RwLock::new(CollectorFeeDetails {
            transaction_fee,
            priority_fee,
        });
        let expected_burn = transaction_fee * bank.burn_percent() / 100;
        let expected_rewards = transaction_fee - expected_burn + priority_fee;

        let collector_id = *bank.leader_id();

        let initial_capitalization = bank.capitalization();
        let initial_collector_balance = bank.get_balance(&collector_id);
        bank.distribute_transaction_fee_details();
        let new_collector_balance = bank.get_balance(&collector_id);

        assert_eq!(
            initial_collector_balance + expected_rewards,
            new_collector_balance
        );
        assert_eq!(
            initial_capitalization - expected_burn,
            bank.capitalization()
        );
        let locked_rewards = bank.rewards.read().unwrap();
        assert_eq!(
            locked_rewards.len(),
            1,
            "There should be one reward distributed"
        );

        let reward_info = &locked_rewards[0];
        assert_eq!(
            reward_info.1.lamports, expected_rewards as i64,
            "The reward amount should match the expected deposit"
        );
        assert_eq!(
            reward_info.1.reward_type,
            RewardType::Fee,
            "The reward type should be Fee"
        );
    }

    #[test]
    fn test_distribute_transaction_fee_details_zero() {
        let genesis = create_genesis_config(0);
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        assert_eq!(
            *bank.collector_fee_details.read().unwrap(),
            CollectorFeeDetails::default()
        );

        let initial_capitalization = bank.capitalization();
        let initial_leader_id_balance = bank.get_balance(bank.leader_id());
        bank.distribute_transaction_fee_details();
        let new_leader_id_balance = bank.get_balance(bank.leader_id());

        assert_eq!(initial_leader_id_balance, new_leader_id_balance);
        assert_eq!(initial_capitalization, bank.capitalization());
        let locked_rewards = bank.rewards.read().unwrap();
        assert!(
            locked_rewards.is_empty(),
            "There should be no rewards distributed"
        );
    }

    #[test]
    fn test_distribute_transaction_fee_details_overflow_failure() {
        let initial_balance = 1000;
        let genesis = create_genesis_config_with_leader(0, &pubkey::new_rand(), initial_balance);
        let mut bank = Bank::new_for_tests(&genesis.genesis_config);
        let transaction_fee = 100;
        let priority_fee = 200;
        bank.collector_fee_details = RwLock::new(CollectorFeeDetails {
            transaction_fee,
            priority_fee,
        });

        let collector_id = *bank.leader_id();

        // ensure that account balance will overflow and fee distribution will fail
        let mut account = bank.get_account(&collector_id).unwrap_or_default();
        account.set_lamports(u64::MAX);
        bank.store_account(&collector_id, &account);

        let initial_capitalization = bank.capitalization();
        let initial_collector_balance = bank.get_balance(&collector_id);
        bank.distribute_transaction_fee_details();
        let new_collector_balance = bank.get_balance(&collector_id);

        assert_eq!(initial_collector_balance, new_collector_balance);
        assert_eq!(
            initial_capitalization - transaction_fee - priority_fee,
            bank.capitalization()
        );
        let locked_rewards = bank.rewards.read().unwrap();
        assert!(
            locked_rewards.is_empty(),
            "There should be no rewards distributed"
        );
    }
}
