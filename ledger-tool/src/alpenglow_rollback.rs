use {
    solana_account::{AccountSharedData, ReadableAccount as _, state_traits::StateMutWincode as _},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_stake_interface::{self as stake, state::StakeStateV2},
    solana_sysvar::epoch_rewards::{self, EpochRewards},
    solana_vote_program::{self, vote_state::VoteStateVersions},
    thiserror::Error,
};

/// Counts and identifies every reward-state change made by an Alpenglow rollback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AlpenglowRollbackReport {
    pub vote_accounts_reset: usize,
    pub stake_accounts_reset: usize,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum AlpenglowRollbackError {
    #[error(
        "partitioned epoch rewards are active: {distributed_rewards} of {total_rewards} lamports \
         have been distributed"
    )]
    ActiveEpochRewards {
        distributed_rewards: u64,
        total_rewards: u64,
    },

    #[error("the EpochRewards sysvar account is malformed")]
    InvalidEpochRewardsSysvar,

    #[error("failed to scan vote accounts: {0}")]
    VoteAccountScan(String),

    #[error("failed to scan stake accounts: {0}")]
    StakeAccountScan(String),

    #[error(
        "vote account {address} still has {pending_delegator_rewards} lamports of pending \
         delegator rewards"
    )]
    PendingDelegatorRewards {
        address: Pubkey,
        pending_delegator_rewards: u64,
    },
}

pub(crate) struct PreparedAlpenglowRollback {
    accounts: Vec<(Pubkey, AccountSharedData)>,
    report: AlpenglowRollbackReport,
}

impl PreparedAlpenglowRollback {
    pub(crate) fn apply(self, bank: &Bank) -> AlpenglowRollbackReport {
        if !self.accounts.is_empty() {
            bank.store_accounts((bank.slot(), self.accounts.as_slice()), None);
        }
        self.report
    }
}

/// Prepare Tower reward cursors without changing materialized balances.
pub(crate) fn preflight_alpenglow_rollback(
    bank: &Bank,
) -> Result<PreparedAlpenglowRollback, AlpenglowRollbackError> {
    require_inactive_epoch_rewards(bank)?;

    let rollback_epoch = bank.epoch_schedule().get_epoch(bank.slot() + 1);
    let mut report = AlpenglowRollbackReport::default();

    let mut vote_accounts = bank
        .get_program_accounts(&solana_vote_program::id())
        .map_err(|err| AlpenglowRollbackError::VoteAccountScan(err.to_string()))?;
    vote_accounts.sort_unstable_by_key(|(address, _account)| *address);

    let mut prepared_accounts = Vec::new();
    for (address, account) in vote_accounts {
        if let Some(replacement) = prepare_vote_account(address, &account, rollback_epoch)? {
            report.vote_accounts_reset += 1;
            prepared_accounts.push((address, replacement));
        }
    }

    let mut stake_accounts = bank
        .get_program_accounts(&stake::program::id())
        .map_err(|err| AlpenglowRollbackError::StakeAccountScan(err.to_string()))?;
    stake_accounts.sort_unstable_by_key(|(address, _account)| *address);

    for (address, account) in stake_accounts {
        if let Some(replacement) = prepare_stake_account(&account) {
            report.stake_accounts_reset += 1;
            prepared_accounts.push((address, replacement));
        }
    }

    Ok(PreparedAlpenglowRollback {
        accounts: prepared_accounts,
        report,
    })
}

fn require_inactive_epoch_rewards(bank: &Bank) -> Result<(), AlpenglowRollbackError> {
    let Some(account) = bank.get_account(&epoch_rewards::id()) else {
        // This sysvar did not exist on older ledgers. Absence is equivalent to the runtime's
        // default, inactive EpochRewards value.
        return Ok(());
    };
    let epoch_rewards: EpochRewards = account
        .state()
        .map_err(|_| AlpenglowRollbackError::InvalidEpochRewardsSysvar)?;
    if epoch_rewards.active {
        return Err(AlpenglowRollbackError::ActiveEpochRewards {
            distributed_rewards: epoch_rewards.distributed_rewards,
            total_rewards: epoch_rewards.total_rewards,
        });
    }
    Ok(())
}

fn prepare_vote_account(
    address: Pubkey,
    account: &AccountSharedData,
    rollback_epoch: Epoch,
) -> Result<Option<AccountSharedData>, AlpenglowRollbackError> {
    let Ok(mut vote_state): Result<VoteStateVersions, _> = account.state() else {
        return Ok(None);
    };

    if vote_state.is_uninitialized() {
        return Ok(None);
    }

    // Match StakesCache::check_and_store(): merely being owned by the vote program and
    // deserializing as VoteStateVersions is not enough to participate in rewards.  Invalid-size
    // and otherwise non-cacheable accounts are inert protocol state, so an unrelated account
    // assigned to the vote program must not be able to prevent an emergency rollback.
    if !VoteStateVersions::is_correct_size_and_initialized(account.data())
        || solana_vote::vote_account::VoteAccount::try_from(account.clone()).is_err()
    {
        return Ok(None);
    }

    if let VoteStateVersions::V4(vote_state_v4) = &vote_state
        && vote_state_v4.pending_delegator_rewards != 0
    {
        return Err(AlpenglowRollbackError::PendingDelegatorRewards {
            address,
            pending_delegator_rewards: vote_state_v4.pending_delegator_rewards,
        });
    }
    let epoch_credits = match &mut vote_state {
        VoteStateVersions::Uninitialized => unreachable!("checked is_uninitialized above"),
        VoteStateVersions::V1_14_11(vote_state) => &mut vote_state.epoch_credits,
        VoteStateVersions::V3(vote_state) => &mut vote_state.epoch_credits,
        VoteStateVersions::V4(vote_state) => &mut vote_state.epoch_credits,
    };
    *epoch_credits = vec![(rollback_epoch, 0, 0)];

    let mut replacement = account.clone();
    replacement
        .set_state(&vote_state)
        .expect("shortened vote state must fit");
    Ok(Some(replacement))
}

fn prepare_stake_account(account: &AccountSharedData) -> Option<AccountSharedData> {
    let stake_state: StakeStateV2 = account.state().ok()?;
    let StakeStateV2::Stake(meta, mut stake, stake_flags) = stake_state else {
        return None;
    };
    stake.credits_observed = 0;

    let mut replacement = account.clone();
    replacement
        .set_state(&StakeStateV2::Stake(meta, stake, stake_flags))
        .expect("unchanged-size stake state must fit");
    Some(replacement)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::WritableAccount as _,
        solana_clock::Clock,
        solana_runtime::{bank::Bank, genesis_utils::create_genesis_config},
        solana_stake_interface::{
            stake_flags::StakeFlags,
            state::{Delegation, Meta, Stake},
        },
        solana_vote_program::vote_state::{VoteInit, VoteStateV3, VoteStateV4},
    };

    fn new_vote_account(vote_state: VoteStateVersions, lamports: u64) -> AccountSharedData {
        AccountSharedData::new_data_with_space(
            lamports,
            &vote_state,
            VoteStateV4::size_of(),
            &solana_vote_program::id(),
        )
        .unwrap()
    }

    fn deserialize_vote_state(account: &AccountSharedData) -> VoteStateVersions {
        account.state().unwrap()
    }

    fn initialized_v3(epoch_credits: Vec<(Epoch, u64, u64)>) -> VoteStateVersions {
        let vote_init = VoteInit {
            node_pubkey: Pubkey::new_unique(),
            authorized_voter: Pubkey::new_unique(),
            authorized_withdrawer: Pubkey::new_unique(),
            commission: 17,
        };
        let mut vote_state = VoteStateV3::new(&vote_init, &Clock::default());
        vote_state.root_slot = Some(41);
        vote_state.epoch_credits = epoch_credits;
        VoteStateVersions::new_v3(vote_state)
    }

    fn new_stake_account(stake_state: &StakeStateV2, lamports: u64) -> AccountSharedData {
        AccountSharedData::new_data_with_space(
            lamports,
            stake_state,
            StakeStateV2::size_of(),
            &stake::program::id(),
        )
        .unwrap()
    }

    fn deserialize_stake_state(account: &AccountSharedData) -> StakeStateV2 {
        account.state().unwrap()
    }

    #[test]
    fn multi_epoch_vote_history_and_migration_marker_are_reset() {
        let address = Pubkey::new_unique();
        let migration_marker = (Epoch::MAX, u64::MAX, u64::MAX);
        let original_state =
            initialized_v3(vec![(3, 11, 7), migration_marker, (4, 19, 0), (5, 31, 19)]);
        let mut account = new_vote_account(original_state, 123_456);
        account.set_executable(true);
        account.set_rent_epoch(55);

        let replacement = prepare_vote_account(address, &account, 9).unwrap().unwrap();
        let VoteStateVersions::V3(vote_state) = deserialize_vote_state(&replacement) else {
            unreachable!();
        };
        assert_eq!(vote_state.epoch_credits, vec![(9, 0, 0)]);
        assert_eq!(replacement.lamports(), account.lamports());
        assert_eq!(replacement.executable(), account.executable());
        assert_eq!(replacement.rent_epoch(), account.rent_epoch());
    }

    #[test]
    fn v4_pending_delegator_rewards_abort_preparation() {
        let address = Pubkey::new_unique();
        let vote_state = VoteStateV4 {
            pending_delegator_rewards: 42,
            epoch_credits: vec![(7, 99, 80)],
            ..VoteStateV4::default()
        };
        let account = new_vote_account(VoteStateVersions::new_v4(vote_state), 500);

        assert_eq!(
            prepare_vote_account(address, &account, 8),
            Err(AlpenglowRollbackError::PendingDelegatorRewards {
                address,
                pending_delegator_rewards: 42,
            })
        );
    }

    #[test]
    fn parseable_but_noncacheable_vote_account_is_malformed() {
        let address = Pubkey::new_unique();
        let account = AccountSharedData::new_data(
            1,
            &initialized_v3(vec![(1, 10, 0)]),
            &solana_vote_program::id(),
        )
        .unwrap();
        assert!(
            prepare_vote_account(address, &account, 2)
                .unwrap()
                .is_none()
        );
        assert!(
            prepare_vote_account(
                address,
                &new_vote_account(VoteStateVersions::Uninitialized, 1),
                2
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn stake_cursor_is_reset_without_changing_delegation_or_account_fields() {
        let original_stake = Stake {
            delegation: Delegation {
                voter_pubkey: Pubkey::new_unique(),
                stake: 99_000,
                activation_epoch: 3,
                deactivation_epoch: 20,
                ..Delegation::default()
            },
            credits_observed: 1_234,
        };
        let original_state =
            StakeStateV2::Stake(Meta::default(), original_stake, StakeFlags::empty());
        let mut account = new_stake_account(&original_state, 456_789);
        account.set_rent_epoch(77);

        let replacement = prepare_stake_account(&account).unwrap();
        let StakeStateV2::Stake(_, normalized_stake, _) = deserialize_stake_state(&replacement)
        else {
            unreachable!();
        };

        assert_eq!(normalized_stake.credits_observed, 0);
        assert_eq!(normalized_stake.delegation, original_stake.delegation);
        assert_eq!(replacement.lamports(), account.lamports());
        assert_eq!(replacement.rent_epoch(), account.rent_epoch());
        assert_eq!(
            deserialize_stake_state(&account)
                .stake()
                .unwrap()
                .credits_observed,
            1_234
        );
        assert!(
            prepare_stake_account(&new_stake_account(
                &StakeStateV2::Initialized(Meta::default()),
                1
            ))
            .is_none()
        );
    }

    #[test]
    fn active_epoch_rewards_abort_before_account_preparation() {
        let bank = Bank::new_for_tests(&create_genesis_config(1_000_000).genesis_config);
        let epoch_rewards = EpochRewards {
            total_rewards: 100,
            distributed_rewards: 40,
            active: true,
            ..EpochRewards::default()
        };
        let account =
            AccountSharedData::new_data(1, &epoch_rewards, &solana_sdk_ids::sysvar::id()).unwrap();
        bank.store_account(&epoch_rewards::id(), &account);

        assert!(matches!(
            preflight_alpenglow_rollback(&bank),
            Err(AlpenglowRollbackError::ActiveEpochRewards {
                distributed_rewards: 40,
                total_rewards: 100,
            })
        ));
    }

    #[test]
    fn apply_stores_all_replacements_and_updates_vote_cache() {
        let source = Bank::new_for_tests(&create_genesis_config(1_000_000).genesis_config);
        let vote_address = Pubkey::new_unique();
        let stake_address = Pubkey::new_unique();
        let vote_account = new_vote_account(initialized_v3(vec![(0, 15, 0), (1, 30, 15)]), 10_000);
        let stake_account = new_stake_account(
            &StakeStateV2::Stake(
                Meta::default(),
                Stake {
                    delegation: Delegation {
                        voter_pubkey: vote_address,
                        stake: 5_000,
                        ..Delegation::default()
                    },
                    credits_observed: 30,
                },
                StakeFlags::empty(),
            ),
            20_000,
        );
        source.store_accounts(
            (
                source.slot(),
                &[
                    (vote_address, vote_account.clone()),
                    (stake_address, stake_account.clone()),
                ][..],
            ),
            None,
        );

        let prepared = preflight_alpenglow_rollback(&source).unwrap();
        let report = prepared.apply(&source);
        assert!(report.vote_accounts_reset >= 1);
        assert!(report.stake_accounts_reset >= 1);

        let VoteStateVersions::V3(vote_state) =
            deserialize_vote_state(&source.get_account(&vote_address).unwrap())
        else {
            unreachable!();
        };
        assert_eq!(
            vote_state.epoch_credits,
            vec![(source.epoch_schedule().get_epoch(source.slot() + 1), 0, 0)]
        );
        let StakeStateV2::Stake(_, stake, _) =
            deserialize_stake_state(&source.get_account(&stake_address).unwrap())
        else {
            unreachable!();
        };
        assert_eq!(stake.credits_observed, 0);
        let cached_vote_accounts = source.vote_accounts();
        let cached_vote_account = cached_vote_accounts.get(&vote_address).unwrap().1.account();
        let VoteStateVersions::V3(cached_vote_state) = deserialize_vote_state(cached_vote_account)
        else {
            unreachable!();
        };
        assert_eq!(cached_vote_state.epoch_credits, vote_state.epoch_credits);
    }
}
