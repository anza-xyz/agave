#![cfg(test)]
mod tests {

    use {
        crate::{
            bank::{Bank, VAT_TO_BURN_PER_EPOCH},
            block_component_processor::vote_reward::{
                VoteState, increment_credits,
                tests::{new_bank_from_parent, set_commission},
            },
            genesis_utils::{
                ValidatorVoteKeypairs, activate_all_features, create_genesis_config_with_leader_ex,
                create_validator,
            },
            inflation_rewards::commission_split,
            stake_utils,
        },
        agave_feature_set::FeatureSet,
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::Block,
        },
        solana_account::{Account, ReadableAccount},
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_cluster_type::ClusterType,
        solana_epoch_schedule::EpochSchedule,
        solana_fee_calculator::FeeRateGovernor,
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_signer::Signer,
        solana_stake_interface::state::StakeStateV2,
        std::{collections::HashMap, num::NonZero, sync::Arc},
        test_case::test_matrix,
    };

    fn new_bank_for_tests(leader: SlotLeader, genesis_config: &GenesisConfig) -> Bank {
        let bank = Bank::new_with_paths_for_tests(genesis_config, None, vec![], Some(leader));
        assert_eq!(*bank.leader(), leader);
        bank
    }

    fn install_add_stakers(
        genesis_config: &mut GenesisConfig,
        validators: &[ValidatorVoteKeypairs],
        num_add_stakers: u64,
        lamports: u64,
    ) -> HashMap<Pubkey, Vec<Pubkey>> {
        let mut ret = validators
            .iter()
            .map(|keypair| {
                (
                    keypair.vote_keypair.pubkey(),
                    vec![keypair.stake_keypair.pubkey()],
                )
            })
            .collect::<HashMap<_, _>>();
        for validator in validators {
            let voter_pubkey = validator.vote_keypair.pubkey();
            let vote_account = genesis_config
                .accounts
                .get(&voter_pubkey)
                .unwrap()
                .clone()
                .into();

            for i in 0..num_add_stakers {
                let staker_keypair = Keypair::new();
                let staker_pubkey = staker_keypair.pubkey();
                let account = Account::from(stake_utils::create_stake_account(
                    &staker_pubkey,
                    &voter_pubkey,
                    &vote_account,
                    &genesis_config.rent,
                    lamports + (i + 1) * lamports,
                ));
                genesis_config.accounts.insert(staker_pubkey, account);
                ret.get_mut(&voter_pubkey).unwrap().push(staker_pubkey);
            }
        }
        ret
    }

    enum PayType {
        Both {
            ag_credits: NonZero<u64>,
            tower_credits: u64,
        },
        Tower(u64),
        None,
    }

    impl PayType {
        fn tower(&self) -> u64 {
            match self {
                Self::Both { tower_credits, .. } => *tower_credits,
                Self::Tower(t) => *t,
                Self::None => 0,
            }
        }

        fn ag(&self) -> Option<NonZero<u64>> {
            match self {
                Self::Both { ag_credits, .. } => Some(*ag_credits),
                Self::Tower(_) | Self::None => None,
            }
        }
    }

    struct State {
        pay_type: PayType,
        commission_bps: u16,
        validators: Vec<ValidatorVoteKeypairs>,
        /// key is validator pubkey, value is list of stakers.
        stakers: HashMap<Pubkey, Vec<Pubkey>>,
    }

    impl State {
        fn new(
            pay_leader: bool,
            num_validators: u64,
            num_add_stakers: u64,
            commission_bps: u16,
            pay_type: PayType,
        ) -> (Self, Bank) {
            let lamports = LAMPORTS_PER_SOL * 20;
            let mint_keypair = Keypair::new();
            let validators = (0..num_validators)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            let leader = if pay_leader {
                let vote_pubkey = validators[0].vote_keypair.pubkey();
                let node_pubkey = validators[0].node_keypair.pubkey();
                SlotLeader {
                    id: node_pubkey,
                    vote_address: vote_pubkey,
                }
            } else {
                SlotLeader::new_unique()
            };
            let mut genesis_config = create_genesis_config_with_leader_ex(
                lamports,
                &mint_keypair.pubkey(),
                &validators[0].node_keypair.pubkey(),
                &validators[0].vote_keypair.pubkey(),
                &validators[0].stake_keypair.pubkey(),
                Some(validators[0].bls_keypair.public.to_bytes_compressed()),
                lamports,
                lamports,
                FeeRateGovernor::new(0, 0),
                Rent::default(),
                ClusterType::Development,
                &FeatureSet::all_enabled(),
                vec![],
            );
            genesis_config.epoch_schedule = EpochSchedule::without_warmup();
            activate_all_features(&mut genesis_config);
            for (ind, keypair) in validators.iter().enumerate().skip(1) {
                let node_pubkey = keypair.node_keypair.pubkey();
                let vote_pubkey = keypair.vote_keypair.pubkey();
                let stake_pubkey = keypair.stake_keypair.pubkey();
                let bls_pubkey = Some(keypair.bls_keypair.public.to_bytes_compressed());
                let lamports = lamports + ind as u64 * LAMPORTS_PER_SOL;
                let accounts = create_validator(
                    &genesis_config.rent,
                    node_pubkey,
                    lamports,
                    vote_pubkey,
                    lamports,
                    stake_pubkey,
                    lamports,
                    bls_pubkey,
                )
                .into_iter()
                .map(|(pubkey, account)| (pubkey, Account::from(account)));
                genesis_config.accounts.extend(accounts);
            }
            set_commission(&mut genesis_config, &validators, commission_bps);

            let stakers =
                install_add_stakers(&mut genesis_config, &validators, num_add_stakers, lamports);
            let bank_epoch0 = new_bank_for_tests(leader, &genesis_config);
            assert_eq!(bank_epoch0.epoch(), 0);

            (
                Self {
                    pay_type,
                    commission_bps,
                    validators,
                    stakers,
                },
                bank_epoch0,
            )
        }

        fn add_tower_rewards(&self, bank: Arc<Bank>) -> Arc<Bank> {
            let vote_accounts = bank.vote_accounts();
            let updated_accounts = self
                .validators
                .iter()
                .map(|validator| {
                    let vote_pubkey = validator.vote_keypair.pubkey();
                    let mut vote_state = VoteState::try_new(&vote_accounts, vote_pubkey).unwrap();
                    vote_state
                        .handler
                        .increment_credits(bank.epoch(), self.pay_type.tower());
                    vote_state.serialize().unwrap()
                })
                .collect::<Vec<_>>();
            bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
            let slot = bank.slot() + 10;
            new_bank_from_parent(bank, slot)
        }

        fn add_ag_rewards(&self, bank: Arc<Bank>) -> Arc<Bank> {
            let vote_accounts = bank.vote_accounts();
            let updated_accounts = self
                .validators
                .iter()
                .map(|validator| {
                    let vote_pubkey = validator.vote_keypair.pubkey();
                    let mut vote_state = VoteState::try_new(&vote_accounts, vote_pubkey).unwrap();
                    if let Some(ag_credits) = self.pay_type.ag() {
                        increment_credits(
                            vote_state.handler.epoch_credits_mut(),
                            bank.epoch(),
                            bank.epoch(),
                            ag_credits,
                        );
                    }
                    vote_state.serialize().unwrap()
                })
                .collect::<Vec<_>>();
            bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
            let slot = bank.slot() + 10;
            new_bank_from_parent(bank, slot)
        }

        fn get_validator_stake(&self, bank: &Bank, pubkey: &Pubkey) -> u64 {
            let rent_exempt_reserve = bank
                .rent_collector()
                .rent
                .minimum_balance(StakeStateV2::size_of());
            self.stakers
                .get(pubkey)
                .unwrap()
                .iter()
                .map(|pubkey| {
                    let lamports = bank.get_account(pubkey).unwrap().lamports();
                    lamports - rent_exempt_reserve
                })
                .sum()
        }

        fn calculate_tower_rewards(
            &self,
            payout_bank: &Bank,
            reward_bank: &Bank,
        ) -> HashMap<Pubkey, u64> {
            if self.pay_type.tower() == 0 {
                let mut ret = HashMap::new();
                for stakers in self.stakers.values() {
                    for staker in stakers {
                        ret.insert(*staker, 0);
                    }
                }
                return ret;
            }

            let total_points = self
                .validators
                .iter()
                .map(|validator| {
                    let vote_pubkey = validator.vote_keypair.pubkey();
                    let validator_stake = self.get_validator_stake(reward_bank, &vote_pubkey);
                    let points = validator_stake * self.pay_type.tower();
                    points as u128
                })
                .sum::<u128>();

            let epoch_inflation = payout_bank.calculate_epoch_inflation_rewards(
                reward_bank.capitalization(),
                reward_bank.epoch(),
            );
            let genesis_cert = payout_bank.get_alpenglow_genesis_certificate().unwrap();
            let first_slot_in_reward_epoch = payout_bank
                .epoch_schedule
                .get_first_slot_in_epoch(reward_bank.epoch());
            let num_tower_slots = genesis_cert.cert_type.slot() - first_slot_in_reward_epoch + 1;
            let total_slots = reward_bank.epoch_schedule.slots_per_epoch;

            let rent_exempt_reserve = reward_bank
                .rent_collector()
                .rent
                .minimum_balance(StakeStateV2::size_of());
            let mut ret = HashMap::new();
            for stakers in self.stakers.values() {
                for staker in stakers {
                    let (initial_lamports, _final_lamports) =
                        self.get_initial_and_final_lamports(reward_bank, payout_bank, staker);
                    if initial_lamports <= LAMPORTS_PER_SOL + rent_exempt_reserve {
                        continue;
                    }
                    let stake = initial_lamports - rent_exempt_reserve;
                    let points = (stake * self.pay_type.tower()) as u128;
                    let reward = points
                        .checked_mul(u128::from(epoch_inflation))
                        .unwrap()
                        .checked_div(total_points)
                        .unwrap()
                        .checked_mul(num_tower_slots as u128)
                        .unwrap()
                        .checked_div(total_slots as u128)
                        .unwrap()
                        .try_into()
                        .unwrap();
                    ret.insert(*staker, reward);
                }
            }
            ret
        }

        fn get_initial_and_final_lamports(
            &self,
            reward_bank: &Bank,
            payout_bank: &Bank,
            pubkey: &Pubkey,
        ) -> (u64, u64) {
            assert!(payout_bank.slot() > reward_bank.slot());
            assert!(payout_bank.epoch() > reward_bank.epoch());
            let initial_lamports = reward_bank.get_account(pubkey).unwrap().lamports();
            let final_lamports = payout_bank.get_account(pubkey).unwrap().lamports();
            (initial_lamports, final_lamports)
        }

        fn validate_stakers(
            &self,
            reward_bank: &Bank,
            payout_bank: &Bank,
            voter_pubkey: &Pubkey,
            tower_rewards: &HashMap<Pubkey, u64>,
        ) -> u64 {
            assert!(payout_bank.slot() > reward_bank.slot());
            assert!(payout_bank.epoch() > reward_bank.epoch());
            let rent_exempt_reserve = reward_bank
                .rent_collector()
                .rent
                .minimum_balance(StakeStateV2::size_of());
            let validator_stake = self.get_validator_stake(reward_bank, voter_pubkey);
            let mut expected_validator_reward = 0;
            for staker_pubkey in self.stakers.get(voter_pubkey).unwrap().iter() {
                let (initial_lamports, final_lamports) =
                    self.get_initial_and_final_lamports(reward_bank, payout_bank, staker_pubkey);
                if initial_lamports <= LAMPORTS_PER_SOL + rent_exempt_reserve {
                    continue;
                }
                let stake = initial_lamports - rent_exempt_reserve;
                let stake_weighted_tower = *tower_rewards.get(staker_pubkey).unwrap();
                let stake_weighted_ag =
                    self.pay_type.ag().map(NonZero::get).unwrap_or(0) * stake / validator_stake;
                let stake_weighted_reward = stake_weighted_tower + stake_weighted_ag;
                let (voter_reward, staker_reward, is_split) =
                    commission_split(self.commission_bps, stake_weighted_reward);
                assert!(is_split);
                assert_eq!(
                    staker_reward,
                    final_lamports - initial_lamports,
                    "final={final_lamports}; initial={initial_lamports}"
                );
                expected_validator_reward += voter_reward;
            }
            expected_validator_reward
        }

        fn validate_validator_reward(
            &self,
            reward_bank: &Bank,
            payout_bank: &Bank,
            tower_rewards: &HashMap<Pubkey, u64>,
            vote_pubkey: Pubkey,
        ) {
            let expected_validator_reward =
                self.validate_stakers(reward_bank, payout_bank, &vote_pubkey, tower_rewards);

            let (initial_lamports, final_lamports) =
                self.get_initial_and_final_lamports(reward_bank, payout_bank, &vote_pubkey);
            let diff = final_lamports
                + VAT_TO_BURN_PER_EPOCH * (payout_bank.epoch() - reward_bank.epoch())
                - initial_lamports;
            assert_eq!(expected_validator_reward, diff);
        }

        fn validate_rewards(&self, reward_bank: &Bank, payout_bank: &Bank) {
            let tower_rewards = self.calculate_tower_rewards(payout_bank, reward_bank);
            for validator in self.validators.iter() {
                let vote_pubkey = validator.vote_keypair.pubkey();
                self.validate_validator_reward(
                    reward_bank,
                    payout_bank,
                    &tower_rewards,
                    vote_pubkey,
                );
            }
        }
    }

    /// Progresses the bank a few times to pay out the rewards.
    fn progress_bank_for_payout(mut bank: Arc<Bank>) -> Arc<Bank> {
        for _ in 0..10 {
            let slot = bank.slot() + 1;
            bank = new_bank_from_parent(bank, slot);
        }
        bank
    }

    #[test_matrix([true, false], [1_000, 5_000], [0, 10], [PayType::Both{ag_credits: NonZero::new(1023).unwrap(), tower_credits:532}, PayType::Tower(383), PayType::None])]
    fn test_migration_epoch(
        pay_leader: bool,
        commission_bps: u16,
        num_add_stakers: u64,
        pay_type: PayType,
    ) {
        let num_validators = 5;

        let (state, mut bank_at_tower) = State::new(
            pay_leader,
            num_validators,
            num_add_stakers,
            commission_bps,
            pay_type,
        );
        bank_at_tower.activate_feature(&agave_feature_set::alpenglow::id());
        let (bank_at_tower, _bank_forks) = bank_at_tower.wrap_with_bank_forks_for_tests();

        let first_slot_in_migration_epoch = bank_at_tower
            .epoch_schedule
            .get_first_slot_in_epoch(bank_at_tower.epoch() + 1);
        let bank_at_migration0 = new_bank_from_parent(bank_at_tower, first_slot_in_migration_epoch);

        let bank_with_tower_rewards = state.add_tower_rewards(bank_at_migration0);

        let genesis_cert = Certificate {
            cert_type: CertificateType::Genesis(Block {
                slot: bank_with_tower_rewards.slot(),
                block_id: Hash::default(),
            }),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        };
        bank_with_tower_rewards.set_alpenglow_genesis_certificate(&genesis_cert);
        let bank_with_genesis_cert_slot = bank_with_tower_rewards.slot() + 10_000;
        let bank_with_genesis_cert =
            new_bank_from_parent(bank_with_tower_rewards, bank_with_genesis_cert_slot);

        let bank_with_ag_rewards = state.add_ag_rewards(bank_with_genesis_cert);
        let reward_epoch = bank_with_ag_rewards.epoch();

        let first_slot_in_ag_epoch = bank_with_ag_rewards
            .epoch_schedule
            .get_first_slot_in_epoch(reward_epoch + 1);
        let bank_at_ag = new_bank_from_parent(bank_with_ag_rewards.clone(), first_slot_in_ag_epoch);
        let progressed_bank = progress_bank_for_payout(bank_at_ag);

        state.validate_rewards(&bank_with_ag_rewards, &progressed_bank);
    }
}
