use {
    solana_clock::Slot, solana_metrics::datapoint_info, solana_pubkey::Pubkey,
    std::collections::HashMap,
};

#[derive(Debug, Default)]
pub(crate) struct ContendedAccountsDetails {
    // latest reported accumulated CUs on this account, must be greater than throttlhold and lesser
    // than Account CU limit.
    pub(crate) accumulated_cu: u64,

    // Leader reported number of TXs wanted to lock this account but failed due to limits;
    pub(crate) attempted_tx_count: u64,

    // leader reported accumulated CUs wanted to be added to this account but failed due to limit;
    pub(crate) attempted_total_cus: u64,
}

impl ContendedAccountsDetails {
    fn accumulate(&mut self, value: &ContendedAccountsDetails) {
        // take latest accumulated CUs
        assert!(self.accumulated_cu <= value.accumulated_cu);
        self.accumulated_cu = value.accumulated_cu;
        self.attempted_tx_count = self
            .attempted_tx_count
            .saturating_add(value.attempted_tx_count);
        self.attempted_total_cus = self
            .attempted_total_cus
            .saturating_add(value.attempted_total_cus);
    }
}

#[derive(Debug)]
pub(crate) struct ContendedAccountsStats {
    contended_accounts: HashMap<Pubkey, ContendedAccountsDetails>,
    contended_account_cu_mark: u64,
}

impl ContendedAccountsStats {
    pub(crate) fn new(account_cost_limit: u64) -> Self {
        Self {
            contended_accounts: HashMap::new(),
            // accounts has more than 95% of account_cu_limit is considered as highly contended
            contended_account_cu_mark: account_cost_limit.saturating_mul(95).saturating_div(100),
        }
    }

    pub(crate) fn get_number_of_contended_accounts(&self) -> usize {
        self.contended_accounts.len()
    }

    pub(crate) fn account_is_contended(&self, accumulated_cost: u64) -> bool {
        accumulated_cost >= self.contended_account_cu_mark
    }

    pub(crate) fn accumulate_contended_accounts_stats(
        &mut self,
        account_key: &Pubkey,
        details: &ContendedAccountsDetails,
    ) {
        let contended_account_details = self
            .contended_accounts
            .entry(*account_key)
            .or_insert(ContendedAccountsDetails::default());

        contended_account_details.accumulate(&details);
    }

    pub(crate) fn report_contended_accounts_stats(&self, bank_slot: Slot, is_leader: bool) {
        for (account_key, details) in &self.contended_accounts {
            datapoint_info!(
                "cost_tracker_contended_accounts_stats",
                "is_leader" => is_leader.to_string(),
                ("bank_slot", bank_slot, i64),
                ("contended_account_key", account_key.to_string(), String),
                ("accumulated_cu", details.accumulated_cu, i64),
                ("attempted_tx_count", details.attempted_tx_count, i64),
                ("attempted_total_cus", details.attempted_total_cus, i64),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accumulate_contended_accounts_stats() {
        let key = Pubkey::new_unique();
        let mut contended_accounts_stats = ContendedAccountsStats::new(u64::MAX);

        contended_accounts_stats.accumulate_contended_accounts_stats(
            &key,
            &ContendedAccountsDetails {
                accumulated_cu: 101,
                attempted_tx_count: 2,
                attempted_total_cus: 10,
            },
        );
        assert_eq!(
            contended_accounts_stats
                .contended_accounts
                .get(&key)
                .unwrap()
                .accumulated_cu,
            101
        );
        assert_eq!(
            contended_accounts_stats
                .contended_accounts
                .get(&key)
                .unwrap()
                .attempted_tx_count,
            2
        );
        assert_eq!(
            contended_accounts_stats
                .contended_accounts
                .get(&key)
                .unwrap()
                .attempted_total_cus,
            10
        );

        contended_accounts_stats.accumulate_contended_accounts_stats(
            &key,
            &ContendedAccountsDetails {
                accumulated_cu: 110,
                attempted_tx_count: 1,
                attempted_total_cus: 20,
            },
        );
        assert_eq!(
            contended_accounts_stats
                .contended_accounts
                .get(&key)
                .unwrap()
                .accumulated_cu,
            110
        );
        assert_eq!(
            contended_accounts_stats
                .contended_accounts
                .get(&key)
                .unwrap()
                .attempted_tx_count,
            3
        );
        assert_eq!(
            contended_accounts_stats
                .contended_accounts
                .get(&key)
                .unwrap()
                .attempted_total_cus,
            30
        );
    }
}
