use {
    super::Bank,
    log::info,
    solana_sdk::{
        account::{
            create_account_shared_data_with_fields as create_account, from_account, ReadableAccount,
        },
        sysvar,
    },
};

impl Bank {
    /// Helper fn to log epoch_rewards sysvar
    pub(in crate::bank) fn log_epoch_rewards_sysvar(&self, prefix: &str) {
        if let Some(account) = self.get_account(&sysvar::epoch_rewards::id()) {
            let epoch_rewards: sysvar::epoch_rewards::EpochRewards =
                from_account(&account).unwrap();
            info!(
                "{prefix} epoch_rewards sysvar: {:?}",
                (account.lamports(), epoch_rewards)
            );
        } else {
            info!("{prefix} epoch_rewards sysvar: none");
        }
    }

    /// Create EpochRewards sysvar with calculated rewards
    pub(in crate::bank) fn create_epoch_rewards_sysvar(
        &self,
        total_rewards: u64,
        distributed_rewards: u64,
        distribution_starting_block_height: u64,
    ) {
        assert!(self.is_partitioned_rewards_code_enabled());

        let epoch_rewards = sysvar::epoch_rewards::EpochRewards {
            total_rewards,
            distributed_rewards,
            distribution_starting_block_height,
            active: true,
            ..sysvar::epoch_rewards::EpochRewards::default()
        };

        self.update_sysvar_account(&sysvar::epoch_rewards::id(), |account| {
            let mut inherited_account_fields =
                self.inherit_specially_retained_account_fields(account);

            assert!(total_rewards >= distributed_rewards);
            // set the account lamports to the undistributed rewards
            inherited_account_fields.0 = total_rewards - distributed_rewards;
            create_account(&epoch_rewards, inherited_account_fields)
        });

        self.log_epoch_rewards_sysvar("create");
    }

    /// Update EpochRewards sysvar with distributed rewards
    pub(in crate::bank) fn update_epoch_rewards_sysvar(&self, distributed: u64) {
        assert!(self.is_partitioned_rewards_code_enabled());

        let mut epoch_rewards: sysvar::epoch_rewards::EpochRewards =
            from_account(&self.get_account(&sysvar::epoch_rewards::id()).unwrap()).unwrap();
        epoch_rewards.distribute(distributed);

        self.update_sysvar_account(&sysvar::epoch_rewards::id(), |account| {
            let mut inherited_account_fields =
                self.inherit_specially_retained_account_fields(account);

            let lamports = inherited_account_fields.0;
            assert!(lamports >= distributed);
            inherited_account_fields.0 = lamports - distributed;
            create_account(&epoch_rewards, inherited_account_fields)
        });

        self.log_epoch_rewards_sysvar("update");
    }
}
