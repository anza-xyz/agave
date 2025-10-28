use {
    solana_clock::Slot,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, num::Saturating},
};

#[derive(Debug, Default)]
struct PrioritizationFeeMetrics {
    // Count of writable accounts in slot
    total_writable_accounts_count: u64,

    // Count of writeable accounts with a minimum prioritization fee higher than the minimum transaction
    // fee for this slot.
    relevant_writable_accounts_count: u64,

    // Count of transactions that have non-zero prioritization fee.
    prioritized_transactions_count: Saturating<u64>,

    // Count of transactions that have zero prioritization fee.
    non_prioritized_transactions_count: Saturating<u64>,

    // Count of attempted update on finalized PrioritizationFee
    attempted_update_on_finalized_fee_count: Saturating<u64>,

    // Total transaction fees of non-vote transactions included in this slot.
    total_prioritization_fee: Saturating<u64>,

    // The minimum compute unit price of prioritized transactions in this slot.
    min_compute_unit_price: Option<u64>,

    // The maximum compute unit price of prioritized transactions in this slot.
    max_compute_unit_price: u64,

    // Accumulated time spent on tracking prioritization fee for each slot.
    total_update_elapsed_us: Saturating<u64>,
}

impl PrioritizationFeeMetrics {
    fn accumulate_total_prioritization_fee(&mut self, val: u64) {
        self.total_prioritization_fee += val;
    }

    fn accumulate_total_update_elapsed_us(&mut self, val: u64) {
        self.total_update_elapsed_us += val;
    }

    fn increment_attempted_update_on_finalized_fee_count(&mut self, val: u64) {
        self.attempted_update_on_finalized_fee_count += val;
    }

    fn update_compute_unit_price(&mut self, cu_price: u64) {
        if cu_price == 0 {
            self.non_prioritized_transactions_count += 1;
            return;
        }

        // update prioritized transaction fee metrics.
        self.prioritized_transactions_count += 1;

        self.max_compute_unit_price = self.max_compute_unit_price.max(cu_price);

        self.min_compute_unit_price = Some(
            self.min_compute_unit_price
                .map_or(cu_price, |min_cu_price| min_cu_price.min(cu_price)),
        );
    }

    fn report(&self, slot: Slot) {
        let &PrioritizationFeeMetrics {
            total_writable_accounts_count,
            relevant_writable_accounts_count,
            prioritized_transactions_count: Saturating(prioritized_transactions_count),
            non_prioritized_transactions_count: Saturating(non_prioritized_transactions_count),
            attempted_update_on_finalized_fee_count:
                Saturating(attempted_update_on_finalized_fee_count),
            total_prioritization_fee: Saturating(total_prioritization_fee),
            min_compute_unit_price,
            max_compute_unit_price,
            total_update_elapsed_us: Saturating(total_update_elapsed_us),
        } = self;
        datapoint_info!(
            "block_prioritization_fee",
            ("slot", slot as i64, i64),
            (
                "total_writable_accounts_count",
                total_writable_accounts_count as i64,
                i64
            ),
            (
                "relevant_writable_accounts_count",
                relevant_writable_accounts_count as i64,
                i64
            ),
            (
                "prioritized_transactions_count",
                prioritized_transactions_count as i64,
                i64
            ),
            (
                "non_prioritized_transactions_count",
                non_prioritized_transactions_count as i64,
                i64
            ),
            (
                "attempted_update_on_finalized_fee_count",
                attempted_update_on_finalized_fee_count as i64,
                i64
            ),
            (
                "total_prioritization_fee",
                total_prioritization_fee as i64,
                i64
            ),
            (
                "min_compute_unit_price",
                min_compute_unit_price.unwrap_or(0) as i64,
                i64
            ),
            ("max_compute_unit_price", max_compute_unit_price as i64, i64),
            (
                "total_update_elapsed_us",
                total_update_elapsed_us as i64,
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
/// a transaction is determined by Max( min_compute_unit_price, min_writable_account_fees(key), ...)
#[derive(Debug)]
pub struct PrioritizationFee {
    // The minimum prioritization fee of transactions that landed in this block.
    min_compute_unit_price: u64,

    // The minimum prioritization fee of each writable account in transactions in this block.
    min_writable_account_fees: HashMap<Pubkey, u64>,

    // Default to `false`, set to `true` when a block is completed, therefore the minimum fees recorded
    // are finalized, and can be made available for use (e.g., RPC query)
    is_finalized: bool,

    // slot prioritization fee metrics
    metrics: PrioritizationFeeMetrics,
}

impl Default for PrioritizationFee {
    fn default() -> Self {
        PrioritizationFee {
            min_compute_unit_price: u64::MAX,
            min_writable_account_fees: HashMap::new(),
            is_finalized: false,
            metrics: PrioritizationFeeMetrics::default(),
        }
    }
}

impl PrioritizationFee {
    /// Update self for minimum transaction fee in the block and minimum fee for each writable account.
    pub fn update(
        &mut self,
        compute_unit_price: u64,
        prioritization_fee: u64,
        writable_accounts: Vec<Pubkey>,
    ) {
        let (_, update_us) = measure_us!({
            if !self.is_finalized {
                if compute_unit_price < self.min_compute_unit_price {
                    self.min_compute_unit_price = compute_unit_price;
                }

                for write_account in writable_accounts {
                    self.min_writable_account_fees
                        .entry(write_account)
                        .and_modify(|write_lock_fee| {
                            *write_lock_fee = std::cmp::min(*write_lock_fee, compute_unit_price)
                        })
                        .or_insert(compute_unit_price);
                }

                self.metrics
                    .accumulate_total_prioritization_fee(prioritization_fee);
                self.metrics.update_compute_unit_price(compute_unit_price);
            } else {
                self.metrics
                    .increment_attempted_update_on_finalized_fee_count(1);
            }
        });

        self.metrics.accumulate_total_update_elapsed_us(update_us);
    }

    /// Accounts that have minimum fees lesser or equal to the minimum fee in the block are redundant, they are
    /// removed to reduce memory footprint when mark_block_completed() is called.
    fn prune_irrelevant_writable_accounts(&mut self) {
        self.metrics.total_writable_accounts_count = self.get_writable_accounts_count() as u64;
        self.min_writable_account_fees
            .retain(|_, account_fee| account_fee > &mut self.min_compute_unit_price);
        self.metrics.relevant_writable_accounts_count = self.get_writable_accounts_count() as u64;
    }

    pub fn mark_block_completed(&mut self) -> Result<(), PrioritizationFeeError> {
        if self.is_finalized {
            return Err(PrioritizationFeeError::BlockIsAlreadyFinalized);
        }
        self.prune_irrelevant_writable_accounts();
        self.is_finalized = true;
        Ok(())
    }

    pub fn get_min_compute_unit_price(&self) -> Option<u64> {
        (self.min_compute_unit_price != u64::MAX).then_some(self.min_compute_unit_price)
    }

    pub fn get_writable_account_fee(&self, key: &Pubkey) -> Option<u64> {
        self.min_writable_account_fees.get(key).copied()
    }

    pub fn get_writable_account_fees(&self) -> impl Iterator<Item = (&Pubkey, &u64)> {
        self.min_writable_account_fees.iter()
    }

    pub fn get_writable_accounts_count(&self) -> usize {
        self.min_writable_account_fees.len()
    }

    pub fn is_finalized(&self) -> bool {
        self.is_finalized
    }

    pub fn report_metrics(&self, slot: Slot) {
        self.metrics.report(slot);
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_pubkey::Pubkey};

    #[test]
    fn test_update_compute_unit_price() {
        agave_logger::setup();
        let write_account_a = Pubkey::new_unique();
        let write_account_b = Pubkey::new_unique();
        let write_account_c = Pubkey::new_unique();
        let tx_fee = 10;

        let mut prioritization_fee = PrioritizationFee::default();
        assert!(prioritization_fee.get_min_compute_unit_price().is_none());

        // Assert for 1st transaction
        // [cu_px, write_accounts...]  -->  [block, account_a, account_b, account_c]
        // -----------------------------------------------------------------------
        // [5,   a, b             ]  -->  [5,     5,         5,         nil      ]
        {
            prioritization_fee.update(5, tx_fee, vec![write_account_a, write_account_b]);
            assert_eq!(5, prioritization_fee.get_min_compute_unit_price().unwrap());
            assert_eq!(
                5,
                prioritization_fee
                    .get_writable_account_fee(&write_account_a)
                    .unwrap()
            );
            assert_eq!(
                5,
                prioritization_fee
                    .get_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert!(prioritization_fee
                .get_writable_account_fee(&write_account_c)
                .is_none());
        }

        // Assert for second transaction:
        // [cu_px, write_accounts...]  -->  [block, account_a, account_b, account_c]
        // -----------------------------------------------------------------------
        // [9,      b, c          ]  -->  [5,     5,         5,         9        ]
        {
            prioritization_fee.update(9, tx_fee, vec![write_account_b, write_account_c]);
            assert_eq!(5, prioritization_fee.get_min_compute_unit_price().unwrap());
            assert_eq!(
                5,
                prioritization_fee
                    .get_writable_account_fee(&write_account_a)
                    .unwrap()
            );
            assert_eq!(
                5,
                prioritization_fee
                    .get_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert_eq!(
                9,
                prioritization_fee
                    .get_writable_account_fee(&write_account_c)
                    .unwrap()
            );
        }

        // Assert for third transaction:
        // [cu_px, write_accounts...]  -->  [block, account_a, account_b, account_c]
        // -----------------------------------------------------------------------
        // [2,   a,    c          ]  -->  [2,     2,         5,         2        ]
        {
            prioritization_fee.update(2, tx_fee, vec![write_account_a, write_account_c]);
            assert_eq!(2, prioritization_fee.get_min_compute_unit_price().unwrap());
            assert_eq!(
                2,
                prioritization_fee
                    .get_writable_account_fee(&write_account_a)
                    .unwrap()
            );
            assert_eq!(
                5,
                prioritization_fee
                    .get_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert_eq!(
                2,
                prioritization_fee
                    .get_writable_account_fee(&write_account_c)
                    .unwrap()
            );
        }

        // assert after prune, account a and c should be removed from cache to save space
        {
            prioritization_fee.prune_irrelevant_writable_accounts();
            assert_eq!(1, prioritization_fee.min_writable_account_fees.len());
            assert_eq!(2, prioritization_fee.get_min_compute_unit_price().unwrap());
            assert!(prioritization_fee
                .get_writable_account_fee(&write_account_a)
                .is_none());
            assert_eq!(
                5,
                prioritization_fee
                    .get_writable_account_fee(&write_account_b)
                    .unwrap()
            );
            assert!(prioritization_fee
                .get_writable_account_fee(&write_account_c)
                .is_none());
        }
    }

    #[test]
    fn test_total_prioritization_fee() {
        let mut prioritization_fee = PrioritizationFee::default();
        prioritization_fee.update(0, 10, vec![]);
        assert_eq!(10, prioritization_fee.metrics.total_prioritization_fee.0);

        prioritization_fee.update(10, u64::MAX, vec![]);
        assert_eq!(
            u64::MAX,
            prioritization_fee.metrics.total_prioritization_fee.0
        );

        prioritization_fee.update(10, 100, vec![]);
        assert_eq!(
            u64::MAX,
            prioritization_fee.metrics.total_prioritization_fee.0
        );
    }

    #[test]
    fn test_mark_block_completed() {
        let mut prioritization_fee = PrioritizationFee::default();

        assert!(prioritization_fee.mark_block_completed().is_ok());
        assert!(prioritization_fee.mark_block_completed().is_err());
    }
}
