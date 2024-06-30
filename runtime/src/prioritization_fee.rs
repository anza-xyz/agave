use {
    solana_measure::measure,
    solana_sdk::{clock::Slot, pubkey::Pubkey, saturating_add_assign},
    std::collections::HashMap,
};

#[derive(Debug, Default)]
struct PrioritizationFeeMetrics {
    // Count of writable accounts in slot
    total_writable_accounts_count: u64,

    // Count of writeable accounts with a minimum prioritization fee higher than the minimum transaction
    // fee for this slot.
    relevant_writable_accounts_count: u64,

    // Count of transactions that have non-zero prioritization fee.
    prioritized_transactions_count: u64,

    // Count of transactions that have zero prioritization fee.
    non_prioritized_transactions_count: u64,

    // Count of attempted update on finalized PrioritizationFee
    attempted_update_on_finalized_fee_count: u64,

    // Total prioritization fees included in this slot.
    total_prioritization_fee: u64,

    // The minimum prioritization fee of prioritized transactions in this slot.
    min_prioritization_fee: Option<u64>,

    // The maximum prioritization fee of prioritized transactions in this slot.
    max_prioritization_fee: u64,

    // Accumulated time spent on tracking prioritization fee for each slot.
    total_update_elapsed_us: u64,
}

impl PrioritizationFeeMetrics {
    fn accumulate_total_prioritization_fee(&mut self, val: u64) {
        saturating_add_assign!(self.total_prioritization_fee, val);
    }

    fn accumulate_total_update_elapsed_us(&mut self, val: u64) {
        saturating_add_assign!(self.total_update_elapsed_us, val);
    }

    fn increment_attempted_update_on_finalized_fee_count(&mut self, val: u64) {
        saturating_add_assign!(self.attempted_update_on_finalized_fee_count, val);
    }

    fn update_prioritization_fee(&mut self, fee: u64) {
        if fee == 0 {
            saturating_add_assign!(self.non_prioritized_transactions_count, 1);
            return;
        }

        // update prioritized transaction fee metrics.
        saturating_add_assign!(self.prioritized_transactions_count, 1);

        self.max_prioritization_fee = self.max_prioritization_fee.max(fee);

        self.min_prioritization_fee = Some(
            self.min_prioritization_fee
                .map_or(fee, |min_fee| min_fee.min(fee)),
        );
    }

    fn report(&self, slot: Slot) {
        datapoint_info!(
            "block_prioritization_fee",
            ("slot", slot as i64, i64),
            (
                "total_writable_accounts_count",
                self.total_writable_accounts_count as i64,
                i64
            ),
            (
                "relevant_writable_accounts_count",
                self.relevant_writable_accounts_count as i64,
                i64
            ),
            (
                "prioritized_transactions_count",
                self.prioritized_transactions_count as i64,
                i64
            ),
            (
                "non_prioritized_transactions_count",
                self.non_prioritized_transactions_count as i64,
                i64
            ),
            (
                "attempted_update_on_finalized_fee_count",
                self.attempted_update_on_finalized_fee_count as i64,
                i64
            ),
            (
                "total_prioritization_fee",
                self.total_prioritization_fee as i64,
                i64
            ),
            (
                "min_prioritization_fee",
                self.min_prioritization_fee.unwrap_or(0) as i64,
                i64
            ),
            (
                "max_prioritization_fee",
                self.max_prioritization_fee as i64,
                i64
            ),
            (
                "total_update_elapsed_us",
                self.total_update_elapsed_us as i64,
                i64
            ),
        );
    }
}

#[derive(Debug)]
pub enum PrioritizationFeeError {
    // Not able to get account locks from sanitized transaction, which is required to update block
    // minimum fees.
    FailGetTransactionAccountLocks,

    // Not able to read compute budget details, including compute-unit price, from transaction.
    // Compute-unit price is required to update block minimum fees.
    FailGetComputeBudgetDetails,

    // Block is already finalized, trying to finalize it again is usually unexpected
    BlockIsAlreadyFinalized,
}

/// Block minimum prioritization fee stats, includes the minimum prioritization fee for a transaction in this
/// block; and the minimum fee for each writable account in all transactions in this block. The only relevant
/// write account minimum fees are those greater than the block minimum transaction fee, because the minimum fee needed to land
/// a transaction is determined by Max( min_transaction_fee, min_writable_account_fees(key), ...)
#[derive(Debug, Default)]
pub struct PrioritizationFee {
    // Prioritization fee of transactions that landed in this block.
    transaction_fees: Vec<u64>,

    // Prioritization fee of each writable account in transactions in this block.
    writable_account_fees: HashMap<Pubkey, Vec<u64>>,

    // Default to `false`, set to `true` when a block is completed, therefore the minimum fees recorded
    // are finalized, and can be made available for use (e.g., RPC query)
    is_finalized: bool,

    // slot prioritization fee metrics
    metrics: PrioritizationFeeMetrics,
}

impl PrioritizationFee {
    /// Update self for minimum transaction fee in the block and minimum fee for each writable account.
    pub fn update(&mut self, transaction_fee: u64, writable_accounts: Vec<Pubkey>) {
        let (_, update_time) = measure!(
            {
                if !self.is_finalized {
                    self.transaction_fees.push(transaction_fee);

                    for write_account in writable_accounts {
                        self.writable_account_fees
                            .entry(write_account)
                            .or_default()
                            .push(transaction_fee);
                    }

                    self.metrics
                        .accumulate_total_prioritization_fee(transaction_fee);
                    self.metrics.update_prioritization_fee(transaction_fee);
                } else {
                    self.metrics
                        .increment_attempted_update_on_finalized_fee_count(1);
                }
            },
            "update_time",
        );

        self.metrics
            .accumulate_total_update_elapsed_us(update_time.as_us());
    }

    pub fn mark_block_completed(&mut self) -> Result<(), PrioritizationFeeError> {
        if self.is_finalized {
            return Err(PrioritizationFeeError::BlockIsAlreadyFinalized);
        }
        self.is_finalized = true;

        self.transaction_fees.sort();
        for fees in self.writable_account_fees.values_mut() {
            fees.sort()
        }

        self.metrics.total_writable_accounts_count = self.get_writable_accounts_count() as u64;
        self.metrics.relevant_writable_accounts_count = self.get_writable_accounts_count() as u64;

        Ok(())
    }

    pub fn get_min_transaction_fee(&self) -> Option<u64> {
        self.transaction_fees.first().copied()
    }

    fn get_percentile(fees: &[u64], percentile: u16) -> Option<u64> {
        let index = (percentile as usize).min(9_999) * fees.len() / 10_000;
        fees.get(index).copied()
    }

    pub fn get_transaction_fee(&self, percentile: u16) -> Option<u64> {
        Self::get_percentile(&self.transaction_fees, percentile)
    }

    pub fn get_min_writable_account_fee(&self, key: &Pubkey) -> Option<u64> {
        self.writable_account_fees
            .get(key)
            .and_then(|fees| fees.first().copied())
    }

    pub fn get_writable_account_fee(&self, key: &Pubkey, percentile: u16) -> Option<u64> {
        self.writable_account_fees
            .get(key)
            .and_then(|fees| Self::get_percentile(fees, percentile))
    }

    pub fn get_writable_account_fees(&self) -> impl Iterator<Item = (&Pubkey, &Vec<u64>)> {
        self.writable_account_fees.iter()
    }

    pub fn get_writable_accounts_count(&self) -> usize {
        self.writable_account_fees.len()
    }

    pub fn is_finalized(&self) -> bool {
        self.is_finalized
    }

    pub fn report_metrics(&self, slot: Slot) {
        self.metrics.report(slot);

        // report this slot's min_transaction_fee and top 10 min_writable_account_fees
        let min_transaction_fee = self.get_min_transaction_fee().unwrap_or(0);
        datapoint_info!(
            "block_min_prioritization_fee",
            ("slot", slot as i64, i64),
            ("entity", "block", String),
            ("min_prioritization_fee", min_transaction_fee as i64, i64),
        );

        let mut accounts_fees: Vec<(&Pubkey, u64)> = self
            .get_writable_account_fees()
            .filter_map(|(account, fees)| {
                fees.first()
                    .copied()
                    .map(|min_account_fee| (account, min_transaction_fee.min(min_account_fee)))
            })
            .collect();
        accounts_fees.sort_by(|lh, rh| rh.1.cmp(&lh.1));
        for (account_key, fee) in accounts_fees.into_iter().take(10) {
            datapoint_trace!(
                "block_min_prioritization_fee",
                ("slot", slot as i64, i64),
                ("entity", account_key.to_string(), String),
                ("min_prioritization_fee", fee as i64, i64),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_sdk::pubkey::Pubkey};

    #[test]
    fn test_update_prioritization_fee() {
        solana_logger::setup();
        let write_account_a = Pubkey::new_unique();
        let write_account_b = Pubkey::new_unique();
        let write_account_c = Pubkey::new_unique();

        let mut prioritization_fee = PrioritizationFee::default();
        assert!(prioritization_fee.get_min_transaction_fee().is_none());

        // Assert for 1st transaction
        // [fee, write_accounts...]  -->  [block, account_a, account_b, account_c]
        // -----------------------------------------------------------------------
        // [5,   a, b             ]  -->  [5,     5,         5,         nil      ]
        {
            prioritization_fee.update(5, vec![write_account_a, write_account_b]);
            assert!(prioritization_fee.mark_block_completed().is_ok());

            assert_eq!(5, prioritization_fee.get_min_transaction_fee().unwrap());
            assert_eq!(
                5,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_a)
                    .unwrap()
            );
            assert_eq!(
                5,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert!(prioritization_fee
                .get_min_writable_account_fee(&write_account_c)
                .is_none());

            prioritization_fee.is_finalized = false;
        }

        // Assert for second transaction:
        // [fee, write_accounts...]  -->  [block, account_a, account_b, account_c]
        // -----------------------------------------------------------------------
        // [9,      b, c          ]  -->  [5,     5,         5,         9        ]
        {
            prioritization_fee.update(9, vec![write_account_b, write_account_c]);
            assert!(prioritization_fee.mark_block_completed().is_ok());

            assert_eq!(5, prioritization_fee.get_min_transaction_fee().unwrap());
            assert_eq!(
                5,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_a)
                    .unwrap()
            );
            assert_eq!(
                5,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert_eq!(
                9,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_c)
                    .unwrap()
            );

            prioritization_fee.is_finalized = false;
        }

        // Assert for third transaction:
        // [fee, write_accounts...]  -->  [block, account_a, account_b, account_c]
        // -----------------------------------------------------------------------
        // [2,   a,    c          ]  -->  [2,     2,         5,         2        ]
        {
            prioritization_fee.update(2, vec![write_account_a, write_account_c]);
            assert!(prioritization_fee.mark_block_completed().is_ok());

            assert_eq!(2, prioritization_fee.get_min_transaction_fee().unwrap());
            assert_eq!(
                2,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_a)
                    .unwrap()
            );
            assert_eq!(
                5,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert_eq!(
                2,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_c)
                    .unwrap()
            );

            prioritization_fee.is_finalized = false;
        }

        // assert after sort
        {
            prioritization_fee.update(2, vec![write_account_a, write_account_c]);
            assert!(prioritization_fee.mark_block_completed().is_ok());

            assert_eq!(2, prioritization_fee.get_min_transaction_fee().unwrap());
            assert_eq!(3, prioritization_fee.writable_account_fees.len());
            assert_eq!(
                2,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_a)
                    .unwrap()
            );
            assert_eq!(
                5,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert_eq!(
                2,
                prioritization_fee
                    .get_min_writable_account_fee(&write_account_c)
                    .unwrap()
            );
        }
    }

    #[test]
    fn test_mark_block_completed() {
        let mut prioritization_fee = PrioritizationFee::default();

        assert!(prioritization_fee.mark_block_completed().is_ok());
        assert!(prioritization_fee.mark_block_completed().is_err());
    }
}
