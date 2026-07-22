//! `cost_tracker` keeps tracking transaction cost per chained accounts as well as for entire block
//! The main function is:
//! - try_add, checks the configured limits and records the transaction's cost when it fits.
use {
    crate::{
        block_cost_limits::*, cost_tracker_post_analysis::CostTrackerPostAnalysis,
        transaction_cost::TransactionCost,
    },
    solana_metrics::datapoint_info,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_transaction_error::TransactionError,
    std::{
        collections::{HashMap, hash_map::Entry},
        num::Saturating,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    },
};

const WRITABLE_ACCOUNTS_PER_BLOCK: usize = 4096;

// Capacity of `try_add`'s rollback bitmap.
// For crafted transactions beyond this bound, possible using tooling paths like ledger-tool, rollback stays panic free.
const ROLLBACK_BITMAP_ACCOUNTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostTrackerError {
    /// would exceed block max limit
    WouldExceedBlockMaxLimit,

    /// would exceed account max limit
    WouldExceedAccountMaxLimit,

    /// would exceed account data block limit
    WouldExceedAccountDataBlockLimit,

    /// would exceed account data total limit
    WouldExceedAccountDataTotalLimit,
}

impl From<CostTrackerError> for TransactionError {
    fn from(err: CostTrackerError) -> Self {
        match err {
            CostTrackerError::WouldExceedBlockMaxLimit => Self::WouldExceedMaxBlockCostLimit,
            CostTrackerError::WouldExceedAccountMaxLimit => Self::WouldExceedMaxAccountCostLimit,
            CostTrackerError::WouldExceedAccountDataBlockLimit => {
                Self::WouldExceedAccountDataBlockLimit
            }
            CostTrackerError::WouldExceedAccountDataTotalLimit => {
                Self::WouldExceedAccountDataTotalLimit
            }
        }
    }
}

/// Relevant block costs that were updated after successful `try_add()`
#[derive(Debug, Default)]
pub struct UpdatedCosts {
    pub updated_block_cost: u64,
    // for all write-locked accounts `try_add()` successfully updated, the highest account cost
    // can be useful info.
    pub updated_costliest_account_cost: u64,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CostTrackerLimits {
    pub account_cost: u64,
    pub block_cost: u64,
    // Maximum new account allocation data per block in bytes.
    pub allocated_data_size: u64,
}

impl CostTrackerLimits {
    const MAX: Self = Self::new(u64::MAX, u64::MAX, u64::MAX);

    pub const fn new(account_cost: u64, block_cost: u64, allocated_data_size: u64) -> Self {
        Self {
            account_cost,
            block_cost,
            allocated_data_size,
        }
    }
}

impl Default for CostTrackerLimits {
    fn default() -> Self {
        const _: () = assert!(MAX_WRITABLE_ACCOUNT_UNITS <= MAX_BLOCK_UNITS);
        Self {
            account_cost: MAX_WRITABLE_ACCOUNT_UNITS,
            block_cost: MAX_BLOCK_UNITS,
            allocated_data_size: MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA,
        }
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug)]
pub struct CostTracker {
    limits: CostTrackerLimits,
    cost_by_writable_accounts: HashMap<Pubkey, u64, ahash::RandomState>,
    block_cost: SharedBlockCost,
    transaction_count: Saturating<u64>,
    allocated_accounts_data_size: Saturating<u64>,
    transaction_signature_count: Saturating<u64>,
    secp256k1_instruction_signature_count: Saturating<u64>,
    ed25519_instruction_signature_count: Saturating<u64>,
    secp256r1_instruction_signature_count: Saturating<u64>,
}

impl Default for CostTracker {
    fn default() -> Self {
        Self {
            limits: CostTrackerLimits::default(),
            cost_by_writable_accounts: HashMap::with_capacity_and_hasher(
                WRITABLE_ACCOUNTS_PER_BLOCK,
                ahash::RandomState::new(),
            ),
            block_cost: SharedBlockCost::new(0),
            transaction_count: Saturating(0),
            allocated_accounts_data_size: Saturating(0),
            transaction_signature_count: Saturating(0),
            secp256k1_instruction_signature_count: Saturating(0),
            ed25519_instruction_signature_count: Saturating(0),
            secp256r1_instruction_signature_count: Saturating(0),
        }
    }
}

impl CostTracker {
    pub fn new_from_parent_limits(&self) -> Self {
        let mut new = Self::default();
        new.set_limits(self.limits);
        new
    }

    /// Get the cost tracker limits.
    pub fn get_limits(&self) -> CostTrackerLimits {
        self.limits
    }

    /// Get the overall account limit.
    pub fn get_account_limit(&self) -> u64 {
        self.limits.account_cost
    }

    /// Get the overall block limit.
    pub fn get_block_limit(&self) -> u64 {
        self.limits.block_cost
    }

    /// Get the overall allocated account data size limit.
    pub fn get_allocated_data_size_limit(&self) -> u64 {
        self.limits.allocated_data_size
    }

    /// allows to adjust limits initiated during construction
    pub fn set_limits(&mut self, limits: CostTrackerLimits) {
        self.limits = limits;
    }

    pub fn set_limits_max(&mut self) {
        self.set_limits(CostTrackerLimits::MAX);
    }

    /// Checks the block and account limits and, if the transaction fits,
    /// adds its cost to the tracker.
    ///
    /// A failed call leaves the tracker unchanged on all validated paths:
    /// account costs applied before the failing account are rolled back,
    /// and the block-level state (including the lock free shared `block_cost`)
    /// is only published after every check has passed.
    pub fn try_add(
        &mut self,
        tx_cost: &TransactionCost<impl TransactionWithMeta>,
    ) -> Result<UpdatedCosts, CostTrackerError> {
        let cost = tx_cost.sum();

        if self.block_cost().saturating_add(cost) > self.limits.block_cost {
            // check against the total package cost
            return Err(CostTrackerError::WouldExceedBlockMaxLimit);
        }

        // check if the transaction itself is more costly than the account_cost_limit
        if cost > self.limits.account_cost {
            return Err(CostTrackerError::WouldExceedAccountMaxLimit);
        }

        let allocated_accounts_data_size =
            self.allocated_accounts_data_size + Saturating(tx_cost.allocated_accounts_data_size());

        if allocated_accounts_data_size.0 > self.limits.allocated_data_size {
            return Err(CostTrackerError::WouldExceedAccountDataBlockLimit);
        }

        // Check each account against account_cost_limit and apply the cost in
        // the same lookup. On failure, undo the applied prefix. `inserted`
        // records which of those lookups created a new entry (a pre-existing
        // entry may hold 0, so the value alone cannot distinguish them).
        let mut updated_costliest_account_cost = 0;
        let mut inserted = [0u64; ROLLBACK_BITMAP_ACCOUNTS / u64::BITS as usize];
        for (index, account_key) in tx_cost.writable_accounts().enumerate() {
            let new_account_cost = match self.cost_by_writable_accounts.entry(*account_key) {
                Entry::Occupied(mut entry) => {
                    let new_account_cost = entry.get().saturating_add(cost);
                    if new_account_cost > self.limits.account_cost {
                        None
                    } else {
                        *entry.get_mut() = new_account_cost;
                        Some(new_account_cost)
                    }
                }
                Entry::Vacant(entry) => {
                    // `cost <= limits.account_cost` was checked above, so an
                    // account without chained cost always fits
                    entry.insert(cost);
                    if let Some(word) = inserted.get_mut(index / u64::BITS as usize) {
                        *word |= 1 << (index % u64::BITS as usize);
                    }
                    Some(cost)
                }
            };
            let Some(new_account_cost) = new_account_cost else {
                // the first `index` accounts were applied before this failure
                self.roll_back_applied_costs(tx_cost, cost, index, &inserted);
                return Err(CostTrackerError::WouldExceedAccountMaxLimit);
            };
            updated_costliest_account_cost = updated_costliest_account_cost.max(new_account_cost);
        }

        // every check passed: publish the block-level state
        self.allocated_accounts_data_size = allocated_accounts_data_size;
        self.transaction_count += 1;
        self.transaction_signature_count += tx_cost.num_transaction_signatures();
        self.secp256k1_instruction_signature_count +=
            tx_cost.num_secp256k1_instruction_signatures();
        self.ed25519_instruction_signature_count += tx_cost.num_ed25519_instruction_signatures();
        self.secp256r1_instruction_signature_count +=
            tx_cost.num_secp256r1_instruction_signatures();
        self.block_cost.fetch_add(cost);

        Ok(UpdatedCosts {
            updated_block_cost: self.block_cost(),
            updated_costliest_account_cost,
        })
    }

    /// Undoes the first `num_applied` per account cost applications of a
    /// partially applied transaction. Entries newly created by this call
    /// (per the `inserted` bitmap) are removed, pre existing ones are decremented.
    ///
    /// Tolerates precondition violating inputs from unvalidated (non consensus) callers.
    /// With duplicate keys, only the first occurrence is marked.
    /// Its removal makes the later occurrences undo a no-op.
    /// The net effect (entry absent) still matches the pre-call state.
    ///
    /// Keys past the bitmap capacity are decremented rather than removed, possibly leaving a zero-cost entry.
    fn roll_back_applied_costs(
        &mut self,
        tx_cost: &TransactionCost<impl TransactionWithMeta>,
        cost: u64,
        num_applied: usize,
        inserted: &[u64],
    ) {
        for (index, account_key) in tx_cost.writable_accounts().take(num_applied).enumerate() {
            let was_inserted = inserted
                .get(index / u64::BITS as usize)
                .is_some_and(|word| word & (1 << (index % u64::BITS as usize)) != 0);
            if was_inserted {
                self.cost_by_writable_accounts.remove(account_key);
            } else if let Some(account_cost) = self.cost_by_writable_accounts.get_mut(account_key) {
                *account_cost = account_cost.saturating_sub(cost);
            }
        }
    }

    pub fn update_execution_cost(
        &mut self,
        estimated_tx_cost: &TransactionCost<impl TransactionWithMeta>,
        actual_execution_units: u64,
        actual_loaded_accounts_data_size_cost: u64,
    ) {
        let actual_load_and_execution_units =
            actual_execution_units.saturating_add(actual_loaded_accounts_data_size_cost);
        let estimated_load_and_execution_units = estimated_tx_cost
            .programs_execution_cost()
            .saturating_add(estimated_tx_cost.loaded_accounts_data_size_cost());
        match actual_load_and_execution_units.cmp(&estimated_load_and_execution_units) {
            std::cmp::Ordering::Equal => (),
            std::cmp::Ordering::Greater => {
                self.add_transaction_execution_cost(
                    estimated_tx_cost,
                    actual_load_and_execution_units - estimated_load_and_execution_units,
                );
            }
            std::cmp::Ordering::Less => {
                self.sub_transaction_execution_cost(
                    estimated_tx_cost,
                    estimated_load_and_execution_units - actual_load_and_execution_units,
                );
            }
        }
    }

    pub fn remove(&mut self, tx_cost: &TransactionCost<impl TransactionWithMeta>) {
        self.remove_transaction_cost(tx_cost);
    }

    pub fn block_cost(&self) -> u64 {
        self.block_cost.load()
    }

    pub fn shared_block_cost(&self) -> SharedBlockCost {
        self.block_cost.clone()
    }

    pub fn transaction_count(&self) -> u64 {
        self.transaction_count.0
    }

    pub fn report_stats(
        &self,
        bank_slot: solana_clock::Slot,
        is_leader: bool,
        total_transaction_fee: u64,
        total_priority_fee: u64,
    ) {
        // skip reporting if block is empty
        if self.transaction_count.0 == 0 {
            return;
        }

        let (costliest_account, costliest_account_cost) = self.find_costliest_account();
        let number_of_contended_accounts = self.find_number_of_contended_accounts();

        datapoint_info!(
            "cost_tracker_stats",
            "is_leader" => is_leader.to_string(),
            ("bank_slot", bank_slot, i64),
            ("block_cost", self.block_cost(), i64),
            ("transaction_count", self.transaction_count.0, i64),
            ("number_of_accounts", self.number_of_accounts(), i64),
            ("costliest_account", costliest_account.to_string(), String),
            ("costliest_account_cost", costliest_account_cost, i64),
            (
                "allocated_accounts_data_size",
                self.allocated_accounts_data_size.0,
                i64
            ),
            (
                "transaction_signature_count",
                self.transaction_signature_count.0,
                i64
            ),
            (
                "secp256k1_instruction_signature_count",
                self.secp256k1_instruction_signature_count.0,
                i64
            ),
            (
                "ed25519_instruction_signature_count",
                self.ed25519_instruction_signature_count.0,
                i64
            ),
            (
                "secp256r1_instruction_signature_count",
                self.secp256r1_instruction_signature_count.0,
                i64
            ),
            ("total_transaction_fee", total_transaction_fee, i64),
            ("total_priority_fee", total_priority_fee, i64),
            ("number_of_contended_accounts", number_of_contended_accounts, i64),
        );
    }

    fn find_costliest_account(&self) -> (Pubkey, u64) {
        self.cost_by_writable_accounts
            .iter()
            .max_by_key(|(_, cost)| **cost)
            .map(|(&pubkey, &cost)| (pubkey, cost))
            .unwrap_or_default()
    }

    fn find_number_of_contended_accounts(&self) -> usize {
        // accounts has more than 95% of account_cu_limit is considered as highly contended
        let contended_cost_mark: u64 = self
            .limits
            .account_cost
            .saturating_mul(95)
            .saturating_div(100);

        self.cost_by_writable_accounts
            .values()
            .filter(|&&cost| cost >= contended_cost_mark)
            .count()
    }

    fn remove_transaction_cost(&mut self, tx_cost: &TransactionCost<impl TransactionWithMeta>) {
        let cost = tx_cost.sum();
        self.sub_transaction_execution_cost(tx_cost, cost);
        self.allocated_accounts_data_size -= tx_cost.allocated_accounts_data_size();
        self.transaction_count -= 1;
        self.transaction_signature_count -= tx_cost.num_transaction_signatures();
        self.secp256k1_instruction_signature_count -=
            tx_cost.num_secp256k1_instruction_signatures();
        self.ed25519_instruction_signature_count -= tx_cost.num_ed25519_instruction_signatures();
        self.secp256r1_instruction_signature_count -=
            tx_cost.num_secp256r1_instruction_signatures();
    }

    /// Apply additional actual execution units to cost_tracker
    /// Return the costliest account cost that were updated by `TransactionCost`
    fn add_transaction_execution_cost(
        &mut self,
        tx_cost: &TransactionCost<impl TransactionWithMeta>,
        adjustment: u64,
    ) -> u64 {
        let mut costliest_account_cost = 0;
        for account_key in tx_cost.writable_accounts() {
            let account_cost = self
                .cost_by_writable_accounts
                .entry(*account_key)
                .or_insert(0);
            *account_cost = account_cost.saturating_add(adjustment);
            costliest_account_cost = costliest_account_cost.max(*account_cost);
        }
        self.block_cost.fetch_add(adjustment);

        costliest_account_cost
    }

    /// Subtract extra execution units from cost_tracker
    fn sub_transaction_execution_cost(
        &mut self,
        tx_cost: &TransactionCost<impl TransactionWithMeta>,
        adjustment: u64,
    ) {
        for account_key in tx_cost.writable_accounts() {
            let account_cost = self
                .cost_by_writable_accounts
                .entry(*account_key)
                .or_insert(0);
            *account_cost = account_cost.saturating_sub(adjustment);
        }
        self.block_cost.fetch_sub(adjustment);
    }

    /// count number of none-zero CU accounts
    fn number_of_accounts(&self) -> usize {
        self.cost_by_writable_accounts
            .values()
            .filter(|units| **units > 0)
            .count()
    }
}

/// Implement the trait for the cost tracker
/// This is only used for post-analysis to avoid lock contention
/// Do not use in the hot path
impl CostTrackerPostAnalysis for CostTracker {
    fn get_cost_by_writable_accounts(&self) -> &HashMap<Pubkey, u64, ahash::RandomState> {
        &self.cost_by_writable_accounts
    }
}

/// Wrapper around blockcost to allow fast sharing of the value without locking.
/// Value is read-only outside of cost-tracker.
#[derive(Debug, Clone)]
pub struct SharedBlockCost(Arc<AtomicU64>);

impl SharedBlockCost {
    pub fn new(value: u64) -> Self {
        Self(Arc::new(AtomicU64::new(value)))
    }

    fn fetch_add(&self, value: u64) -> u64 {
        self.0.fetch_add(value, Ordering::Release)
    }

    fn fetch_sub(&self, value: u64) -> u64 {
        self.0.fetch_sub(value, Ordering::Release)
    }

    pub fn load(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::transaction_cost::{WritableKeysTransaction, *},
        solana_keypair::Keypair,
        solana_signer::Signer,
        std::cmp,
    };

    impl CostTracker {
        fn new(account_cost_limit: u64, block_cost_limit: u64) -> Self {
            assert!(account_cost_limit <= block_cost_limit);
            let mut cost_tracker = Self::default();
            cost_tracker.set_limits(CostTrackerLimits {
                account_cost: account_cost_limit,
                block_cost: block_cost_limit,
                ..CostTrackerLimits::default()
            });
            cost_tracker
        }
    }

    fn test_setup() -> Keypair {
        agave_logger::setup();
        Keypair::new()
    }

    fn build_simple_transaction(mint_keypair: &Keypair) -> WritableKeysTransaction {
        WritableKeysTransaction::new(vec![mint_keypair.pubkey()])
    }

    fn build_simple_vote_transaction(mint_keypair: &Keypair) -> WritableKeysTransaction {
        WritableKeysTransaction {
            writable_keys: vec![mint_keypair.pubkey()],
            is_simple_vote: true,
        }
    }

    fn simple_usage_cost_details(
        transaction: &WritableKeysTransaction,
        programs_execution_cost: u64,
    ) -> UsageCostDetails<'_, WritableKeysTransaction> {
        UsageCostDetails {
            transaction,
            signature_cost: 0,
            write_lock_cost: 0,
            data_bytes_cost: 0,
            programs_execution_cost,
            loaded_accounts_data_size_cost: 0,
            allocated_accounts_data_size: 0,
        }
    }

    fn simple_transaction_cost(
        transaction: &WritableKeysTransaction,
        programs_execution_cost: u64,
    ) -> TransactionCost<'_, WritableKeysTransaction> {
        TransactionCost::new(simple_usage_cost_details(
            transaction,
            programs_execution_cost,
        ))
    }

    fn simple_vote_transaction_cost(
        transaction: &WritableKeysTransaction,
    ) -> TransactionCost<'_, WritableKeysTransaction> {
        TransactionCost::new(UsageCostDetails {
            transaction,
            signature_cost: 1,
            write_lock_cost: 2,
            data_bytes_cost: 0,
            programs_execution_cost: solana_vote_program::vote_processor::DEFAULT_COMPUTE_UNITS,
            loaded_accounts_data_size_cost: 8,
            allocated_accounts_data_size: 0,
        })
    }

    #[test]
    fn test_cost_tracker_initialization() {
        let testee = CostTracker::new(10, 11);
        assert_eq!(10, testee.limits.account_cost);
        assert_eq!(11, testee.limits.block_cost);
        assert_eq!(0, testee.cost_by_writable_accounts.len());
        assert_eq!(0, testee.block_cost());
    }

    #[test]
    fn test_cost_tracker_ok_add_one() {
        let mint_keypair = test_setup();
        let tx = build_simple_transaction(&mint_keypair);
        let tx_cost = simple_transaction_cost(&tx, 5);
        let cost = tx_cost.sum();

        // build testee to have capacity for one simple transaction
        let mut testee = CostTracker::new(cost, cost);
        assert!(testee.try_add(&tx_cost).is_ok());
        assert_eq!(cost, testee.block_cost());
        let (_costliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(cost, costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_ok_add_one_vote() {
        let mint_keypair = test_setup();
        let tx = build_simple_vote_transaction(&mint_keypair);
        let tx_cost = simple_vote_transaction_cost(&tx);
        let cost = tx_cost.sum();

        // build testee to have capacity for one simple transaction
        let mut testee = CostTracker::new(cost, cost);
        assert!(testee.try_add(&tx_cost).is_ok());
        assert_eq!(cost, testee.block_cost());
        let (_costliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(cost, costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_add_data() {
        let mint_keypair = test_setup();
        let tx = build_simple_transaction(&mint_keypair);
        let mut tx_cost = simple_transaction_cost(&tx, 5);
        tx_cost
            .usage_cost_details_mut()
            .allocated_accounts_data_size = 1;
        let cost = tx_cost.sum();

        // build testee to have capacity for one simple transaction
        let mut testee = CostTracker::new(cost, cost);
        let old = testee.allocated_accounts_data_size;
        assert!(testee.try_add(&tx_cost).is_ok());
        assert_eq!(old.0 + 1, testee.allocated_accounts_data_size.0);
    }

    #[test]
    fn test_cost_tracker_ok_add_two_same_accounts() {
        let mint_keypair = test_setup();
        // build two transactions with same signed account
        let tx1 = build_simple_transaction(&mint_keypair);
        let tx_cost1 = simple_transaction_cost(&tx1, 5);
        let cost1 = tx_cost1.sum();
        let tx2 = build_simple_transaction(&mint_keypair);
        let tx_cost2 = simple_transaction_cost(&tx2, 5);
        let cost2 = tx_cost2.sum();

        // build testee to have capacity for two simple transactions, with same accounts
        let mut testee = CostTracker::new(cost1 + cost2, cost1 + cost2);
        {
            assert!(testee.try_add(&tx_cost1).is_ok());
        }
        {
            assert!(testee.try_add(&tx_cost2).is_ok());
        }
        assert_eq!(cost1 + cost2, testee.block_cost());
        assert_eq!(1, testee.cost_by_writable_accounts.len());
        let (_ccostliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(cost1 + cost2, costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_ok_add_two_diff_accounts() {
        let mint_keypair = test_setup();
        // build two transactions with diff accounts
        let second_account = Keypair::new();
        let tx1 = build_simple_transaction(&mint_keypair);
        let tx_cost1 = simple_transaction_cost(&tx1, 5);
        let cost1 = tx_cost1.sum();

        let tx2 = build_simple_transaction(&second_account);
        let tx_cost2 = simple_transaction_cost(&tx2, 5);
        let cost2 = tx_cost2.sum();

        // build testee to have capacity for two simple transactions, with same accounts
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2);
        {
            assert!(testee.try_add(&tx_cost1).is_ok());
        }
        {
            assert!(testee.try_add(&tx_cost2).is_ok());
        }
        assert_eq!(cost1 + cost2, testee.block_cost());
        assert_eq!(2, testee.cost_by_writable_accounts.len());
        let (_ccostliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(std::cmp::max(cost1, cost2), costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_chain_reach_limit() {
        let mint_keypair = test_setup();
        // build two transactions with same signed account
        let tx1 = build_simple_transaction(&mint_keypair);
        let tx_cost1 = simple_transaction_cost(&tx1, 5);
        let cost1 = tx_cost1.sum();
        let tx2 = build_simple_transaction(&mint_keypair);
        let tx_cost2 = simple_transaction_cost(&tx2, 5);
        let cost2 = tx_cost2.sum();

        // build testee to have capacity for two simple transactions, but not for same accounts
        let mut testee = CostTracker::new(cmp::min(cost1, cost2), cost1 + cost2);
        // should have room for first transaction
        {
            assert!(testee.try_add(&tx_cost1).is_ok());
        }
        // but no more sapce on the same chain (same signer account)
        {
            assert!(testee.try_add(&tx_cost2).is_err());
        }
    }

    #[test]
    fn test_cost_tracker_reach_limit() {
        let mint_keypair = test_setup();
        // build two transactions with diff accounts
        let second_account = Keypair::new();
        let tx1 = build_simple_transaction(&mint_keypair);
        let tx_cost1 = simple_transaction_cost(&tx1, 5);
        let cost1 = tx_cost1.sum();
        let tx2 = build_simple_transaction(&second_account);
        let tx_cost2 = simple_transaction_cost(&tx2, 5);
        let cost2 = tx_cost2.sum();

        // build testee to have capacity for each chain, but not enough room for both transactions
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2 - 1);
        // should have room for first transaction
        {
            assert!(testee.try_add(&tx_cost1).is_ok());
        }
        // but no more room for package as whole
        {
            assert!(testee.try_add(&tx_cost2).is_err());
        }
    }

    #[test]
    fn test_cost_tracker_vote_transactions_use_regular_limits() {
        let mint_keypair = test_setup();
        // build two mocking vote transactions with diff accounts
        let second_account = Keypair::new();
        let tx1 = build_simple_vote_transaction(&mint_keypair);
        let tx_cost1 = simple_vote_transaction_cost(&tx1);
        let cost1 = tx_cost1.sum();
        let tx2 = build_simple_vote_transaction(&second_account);
        let tx_cost2 = simple_vote_transaction_cost(&tx2);
        let cost2 = tx_cost2.sum();

        // build testee to have capacity for both vote transactions
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2);
        // should have room for first vote
        {
            assert!(testee.try_add(&tx_cost1).is_ok());
        }
        assert!(testee.try_add(&tx_cost2).is_ok());
    }

    #[test]
    fn test_cost_tracker_reach_data_block_limit() {
        let mint_keypair = test_setup();
        // build two transactions with diff accounts
        let second_account = Keypair::new();
        let tx1 = build_simple_transaction(&mint_keypair);
        let mut tx_cost1 = simple_transaction_cost(&tx1, 5);
        let tx2 = build_simple_transaction(&second_account);
        let mut tx_cost2 = simple_transaction_cost(&tx2, 5);
        tx_cost1
            .usage_cost_details_mut()
            .allocated_accounts_data_size = MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA;
        tx_cost2
            .usage_cost_details_mut()
            .allocated_accounts_data_size = MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA + 1;
        let cost1 = tx_cost1.sum();
        let cost2 = tx_cost2.sum();

        // build testee that passes
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2);
        assert!(testee.try_add(&tx_cost1).is_ok());
        // data is too big
        assert!(matches!(
            testee.try_add(&tx_cost2),
            Err(CostTrackerError::WouldExceedAccountDataBlockLimit),
        ));
    }

    #[test]
    fn test_cost_tracker_respects_custom_allocated_data_size_limit() {
        // Setup transaction that allocates 2 bytes.
        let mint_keypair = test_setup();
        let tx = build_simple_transaction(&mint_keypair);
        let mut tx_cost = simple_transaction_cost(&tx, 5);
        tx_cost
            .usage_cost_details_mut()
            .allocated_accounts_data_size = 2;

        // Transaction fits with default limit.
        let mut testee = CostTracker::new(u64::MAX, u64::MAX);
        assert!(testee.try_add(&tx_cost).is_ok());

        // Transaction does not fit with 1B limit.
        testee.set_limits(CostTrackerLimits {
            allocated_data_size: 1,
            ..testee.get_limits()
        });
        assert!(matches!(
            testee.try_add(&tx_cost),
            Err(CostTrackerError::WouldExceedAccountDataBlockLimit),
        ));
    }

    #[test]
    fn test_cost_tracker_remove() {
        let mint_keypair = test_setup();
        // build two transactions with diff accounts
        let second_account = Keypair::new();
        let tx1 = build_simple_transaction(&mint_keypair);
        let tx_cost1 = simple_transaction_cost(&tx1, 5);
        let tx2 = build_simple_transaction(&second_account);
        let tx_cost2 = simple_transaction_cost(&tx2, 5);
        let cost1 = tx_cost1.sum();
        let cost2 = tx_cost2.sum();

        // build testee
        let mut testee = CostTracker::new(cost1 + cost2, cost1 + cost2);

        assert!(testee.try_add(&tx_cost1).is_ok());
        assert!(testee.try_add(&tx_cost2).is_ok());
        assert_eq!(cost1 + cost2, testee.block_cost());

        // removing a tx_cost affects block_cost
        testee.remove(&tx_cost1);
        assert_eq!(cost2, testee.block_cost());

        // add back tx1
        assert!(testee.try_add(&tx_cost1).is_ok());
        assert_eq!(cost1 + cost2, testee.block_cost());

        // cannot add tx1 again, cost limit would be exceeded
        assert!(testee.try_add(&tx_cost1).is_err());
    }

    #[test]
    fn test_cost_tracker_try_add_is_atomic() {
        let acct1 = Pubkey::new_unique();
        let acct2 = Pubkey::new_unique();
        let acct3 = Pubkey::new_unique();
        let cost = 100;
        let account_max = cost * 2;
        let block_max = account_max * 3; // for three accts

        let mut testee = CostTracker::new(account_max, block_max);

        // case 1: a tx writes to 3 accounts, should success, we will have:
        // | acct1 | $cost |
        // | acct2 | $cost |
        // | acct3 | $cost |
        // and block_cost = $cost
        {
            let transaction = WritableKeysTransaction::new(vec![acct1, acct2, acct3]);
            let tx_cost = simple_transaction_cost(&transaction, cost);
            assert!(testee.try_add(&tx_cost).is_ok());
            let (_costliest_account, costliest_account_cost) = testee.find_costliest_account();
            assert_eq!(cost, testee.block_cost());
            assert_eq!(3, testee.cost_by_writable_accounts.len());
            assert_eq!(cost, costliest_account_cost);
        }

        // case 2: add tx writes to acct2 with $cost, should succeed, result to
        // | acct1 | $cost |
        // | acct2 | $cost * 2 |
        // | acct3 | $cost |
        // and block_cost = $cost * 2
        {
            let transaction = WritableKeysTransaction::new(vec![acct2]);
            let tx_cost = simple_transaction_cost(&transaction, cost);
            assert!(testee.try_add(&tx_cost).is_ok());
            let (costliest_account, costliest_account_cost) = testee.find_costliest_account();
            assert_eq!(cost * 2, testee.block_cost());
            assert_eq!(3, testee.cost_by_writable_accounts.len());
            assert_eq!(cost * 2, costliest_account_cost);
            assert_eq!(acct2, costliest_account);
        }

        // case 3: add tx writes to [acct1, acct2], acct2 exceeds limit, should failed atomically,
        // we should still have:
        // | acct1 | $cost |
        // | acct2 | $cost * 2 |
        // | acct3 | $cost |
        // and block_cost = $cost * 2
        {
            let transaction = WritableKeysTransaction::new(vec![acct1, acct2]);
            let tx_cost = simple_transaction_cost(&transaction, cost);
            assert!(testee.try_add(&tx_cost).is_err());
            let (costliest_account, costliest_account_cost) = testee.find_costliest_account();
            assert_eq!(cost * 2, testee.block_cost());
            assert_eq!(3, testee.cost_by_writable_accounts.len());
            assert_eq!(cost * 2, costliest_account_cost);
            assert_eq!(acct2, costliest_account);
            // the pre-existing acct1 entry was decremented back, not removed
            assert_eq!(Some(&cost), testee.cost_by_writable_accounts.get(&acct1));
        }

        // case 4: add tx writes to [acct4 (unseen), acct2], acct2 exceeds limit;
        // the entry freshly inserted for acct4 must be removed by the rollback,
        // leaving the tracker exactly as after case 2
        {
            let acct4 = Pubkey::new_unique();
            let transaction = WritableKeysTransaction::new(vec![acct4, acct2]);
            let tx_cost = simple_transaction_cost(&transaction, cost);
            assert!(matches!(
                testee.try_add(&tx_cost),
                Err(CostTrackerError::WouldExceedAccountMaxLimit)
            ));
            let (costliest_account, costliest_account_cost) = testee.find_costliest_account();
            assert_eq!(cost * 2, testee.block_cost());
            assert_eq!(3, testee.cost_by_writable_accounts.len());
            assert!(!testee.cost_by_writable_accounts.contains_key(&acct4));
            assert_eq!(cost * 2, costliest_account_cost);
            assert_eq!(acct2, costliest_account);
        }
    }

    #[test]
    fn test_try_add_rollback_across_bitmap_words() {
        let cost = 100;
        let hot_account = Pubkey::new_unique();
        let mut testee = CostTracker::new(cost * 2, cost * 1000);

        // drive hot_account to the limit so the next charge fails
        let transaction = WritableKeysTransaction::new(vec![hot_account]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        assert!(testee.try_add(&tx_cost).is_ok());
        assert!(testee.try_add(&tx_cost).is_ok());
        let block_cost_before = testee.block_cost();

        // 100 fresh accounts followed by hot_account, all 100 fresh entries
        // (bitmap words 0 and 1) are inserted before the failure at index 100
        let mut keys: Vec<Pubkey> = (0..100).map(|_| Pubkey::new_unique()).collect();
        keys.push(hot_account);
        let transaction = WritableKeysTransaction::new(keys);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        assert!(matches!(
            testee.try_add(&tx_cost),
            Err(CostTrackerError::WouldExceedAccountMaxLimit)
        ));

        assert_eq!(1, testee.cost_by_writable_accounts.len());
        assert_eq!(
            Some(&(cost * 2)),
            testee.cost_by_writable_accounts.get(&hot_account)
        );
        assert_eq!(block_cost_before, testee.block_cost());
    }

    // Duplicate writable keys are tolerated by the rollback.
    // Check if only the first occurrence is bitmap marked so rollback must net out to the pre-call state
    #[test]
    fn test_try_add_rollback_with_duplicate_keys() {
        let cost = 100;
        let dup = Pubkey::new_unique();
        let hot_account = Pubkey::new_unique();
        let mut testee = CostTracker::new(cost * 4, cost * 1000);

        // drive hot_account to the limit so any further charge fails
        let transaction = WritableKeysTransaction::new(vec![hot_account]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        for _ in 0..4 {
            assert!(testee.try_add(&tx_cost).is_ok());
        }
        let block_cost_before = testee.block_cost();

        // fresh dup - rollback removes the single entry on the first occurrence, the second undo is a no-op
        let transaction = WritableKeysTransaction::new(vec![dup, dup, hot_account]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        assert!(matches!(
            testee.try_add(&tx_cost),
            Err(CostTrackerError::WouldExceedAccountMaxLimit)
        ));
        assert!(!testee.cost_by_writable_accounts.contains_key(&dup));
        assert_eq!(block_cost_before, testee.block_cost());

        // pre-existing dup
        let transaction = WritableKeysTransaction::new(vec![dup]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        assert!(testee.try_add(&tx_cost).is_ok());
        let block_cost_before = testee.block_cost();

        let transaction = WritableKeysTransaction::new(vec![dup, dup, hot_account]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        assert!(matches!(
            testee.try_add(&tx_cost),
            Err(CostTrackerError::WouldExceedAccountMaxLimit)
        ));
        assert_eq!(Some(&cost), testee.cost_by_writable_accounts.get(&dup));
        assert_eq!(block_cost_before, testee.block_cost());
    }

    #[test]
    fn test_adjust_transaction_execution_cost() {
        let acct1 = Pubkey::new_unique();
        let acct2 = Pubkey::new_unique();
        let acct3 = Pubkey::new_unique();
        let cost = 100;
        let account_max = cost * 2;
        let block_max = account_max * 3; // for three accts

        let mut testee = CostTracker::new(account_max, block_max);
        let transaction = WritableKeysTransaction::new(vec![acct1, acct2, acct3]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        let mut expected_block_cost = tx_cost.sum();
        let expected_tx_count = 1;
        assert!(testee.try_add(&tx_cost).is_ok());
        assert_eq!(expected_block_cost, testee.block_cost());
        assert_eq!(expected_tx_count, testee.transaction_count());
        testee
            .cost_by_writable_accounts
            .iter()
            .for_each(|(_key, units)| {
                assert_eq!(expected_block_cost, *units);
            });

        // adjust up
        {
            let adjustment = 50u64;
            testee.add_transaction_execution_cost(&tx_cost, adjustment);
            expected_block_cost += 50;
            assert_eq!(expected_block_cost, testee.block_cost());
            assert_eq!(expected_tx_count, testee.transaction_count());
            testee
                .cost_by_writable_accounts
                .iter()
                .for_each(|(_key, units)| {
                    assert_eq!(expected_block_cost, *units);
                });
        }

        // adjust down
        {
            let adjustment = 50u64;
            testee.sub_transaction_execution_cost(&tx_cost, adjustment);
            expected_block_cost -= 50;
            assert_eq!(expected_block_cost, testee.block_cost());
            assert_eq!(expected_tx_count, testee.transaction_count());
            testee
                .cost_by_writable_accounts
                .iter()
                .for_each(|(_key, units)| {
                    assert_eq!(expected_block_cost, *units);
                });
        }
    }

    #[test]
    fn test_update_execution_cost() {
        let estimated_programs_execution_cost = 100;
        let estimated_loaded_accounts_data_size_cost = 200;
        let number_writeble_accounts = 3;
        let transaction = WritableKeysTransaction::new(
            std::iter::repeat_with(Pubkey::new_unique)
                .take(number_writeble_accounts)
                .collect(),
        );

        let mut usage_cost =
            simple_usage_cost_details(&transaction, estimated_programs_execution_cost);
        usage_cost.loaded_accounts_data_size_cost = estimated_loaded_accounts_data_size_cost;
        let tx_cost = TransactionCost::new(usage_cost);
        // confirm tx_cost is only made up by programs_execution_cost and
        // loaded_accounts_data_size_cost
        let estimated_tx_cost = tx_cost.sum();
        assert_eq!(
            estimated_tx_cost,
            estimated_programs_execution_cost + estimated_loaded_accounts_data_size_cost
        );

        let test_update_cost_tracker =
            |execution_cost_adjust: i64, loaded_accounts_data_size_cost_adjust: i64| {
                let mut cost_tracker = CostTracker::default();
                assert!(cost_tracker.try_add(&tx_cost).is_ok());

                let actual_programs_execution_cost =
                    (estimated_programs_execution_cost as i64 + execution_cost_adjust) as u64;
                let actual_loaded_accounts_data_size_cost =
                    (estimated_loaded_accounts_data_size_cost as i64
                        + loaded_accounts_data_size_cost_adjust) as u64;
                let expected_cost = (estimated_tx_cost as i64
                    + execution_cost_adjust
                    + loaded_accounts_data_size_cost_adjust)
                    as u64;

                cost_tracker.update_execution_cost(
                    &tx_cost,
                    actual_programs_execution_cost,
                    actual_loaded_accounts_data_size_cost,
                );

                assert_eq!(expected_cost, cost_tracker.block_cost());
                assert_eq!(
                    number_writeble_accounts,
                    cost_tracker.cost_by_writable_accounts.len()
                );
                for writable_account_cost in cost_tracker.cost_by_writable_accounts.values() {
                    assert_eq!(expected_cost, *writable_account_cost);
                }
                assert_eq!(1, cost_tracker.transaction_count.0);
            };

        test_update_cost_tracker(0, 0);
        test_update_cost_tracker(0, 9);
        test_update_cost_tracker(0, -9);
        test_update_cost_tracker(9, 0);
        test_update_cost_tracker(9, 9);
        test_update_cost_tracker(9, -9);
        test_update_cost_tracker(-9, 0);
        test_update_cost_tracker(-9, 9);
        test_update_cost_tracker(-9, -9);
    }

    #[test]
    fn test_remove_transaction_cost() {
        let mut cost_tracker = CostTracker::default();

        let cost = 100u64;
        let transaction = WritableKeysTransaction::new(vec![Pubkey::new_unique()]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        cost_tracker.try_add(&tx_cost).unwrap();
        // assert cost_tracker is reverted to default
        assert_eq!(1, cost_tracker.transaction_count.0);
        assert_eq!(1, cost_tracker.number_of_accounts());
        assert_eq!(cost, cost_tracker.block_cost());
        assert_eq!(0, cost_tracker.allocated_accounts_data_size.0);

        cost_tracker.remove_transaction_cost(&tx_cost);
        // assert cost_tracker is reverted to default
        assert_eq!(0, cost_tracker.transaction_count.0);
        assert_eq!(0, cost_tracker.number_of_accounts());
        assert_eq!(0, cost_tracker.block_cost());
        assert_eq!(0, cost_tracker.allocated_accounts_data_size.0);
    }

    #[test]
    fn test_get_cost_by_writable_accounts_post_analysis() {
        let mut cost_tracker = CostTracker::default();
        let cost = 100u64;
        let transaction = WritableKeysTransaction::new(vec![Pubkey::new_unique()]);
        let tx_cost = simple_transaction_cost(&transaction, cost);
        cost_tracker.try_add(&tx_cost).unwrap();
        let cost_by_writable_accounts = cost_tracker.get_cost_by_writable_accounts();
        assert_eq!(1, cost_by_writable_accounts.len());
        assert_eq!(cost, *cost_by_writable_accounts.values().next().unwrap());
        assert_eq!(
            *cost_by_writable_accounts,
            cost_tracker.cost_by_writable_accounts
        );
    }
}
