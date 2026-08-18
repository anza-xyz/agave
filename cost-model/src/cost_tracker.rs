//! `cost_tracker` keeps tracking transaction cost per chained accounts as well as for entire block
//! The main function is:
//! - try_add, checks the configured limits and records the transaction's cost when it fits.
use {
    crate::{
        block_cost_limits::*, cost_tracker_post_analysis::CostTrackerPostAnalysis,
        transaction_cost::TransactionCost,
    },
    solana_pubkey::Pubkey,
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

/// A snapshot of cost-tracker state used for reporting and post-processing.
pub struct CostTrackerStats {
    pub block_cost: u64,
    pub transaction_count: u64,
    pub number_of_accounts: usize,
    pub costliest_account: Pubkey,
    pub costliest_account_cost: u64,
    pub allocated_accounts_data_size: u64,
    pub in_flight_transaction_count: usize,
    pub number_of_contended_accounts: usize,
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
    allocated_accounts_data_size: SharedAllocatedAccountsDataSize,
    /// The number of transactions that have had their estimated cost added to
    /// the tracker, but are still waiting for an update with actual usage or
    /// removal if the transaction does not end up getting committed.
    in_flight_transaction_count: Saturating<usize>,
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
            allocated_accounts_data_size: SharedAllocatedAccountsDataSize::new(0),
            in_flight_transaction_count: Saturating(0),
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

    pub fn in_flight_transaction_count(&self) -> usize {
        self.in_flight_transaction_count.0
    }

    pub fn add_transactions_in_flight(&mut self, in_flight_transaction_count: usize) {
        self.in_flight_transaction_count += in_flight_transaction_count;
    }

    pub fn sub_transactions_in_flight(&mut self, in_flight_transaction_count: usize) {
        self.in_flight_transaction_count -= in_flight_transaction_count
    }

    /// Checks the block and account limits and, if the transaction fits,
    /// adds its cost to the tracker.
    ///
    /// A failed call leaves the tracker equivalent to the pre-call state.
    /// Account costs applied before the failing account are rolled back,
    /// and the block-level state (including the lock free shared `block_cost`)
    /// is only published after every check has passed.
    pub fn try_add<'a>(
        &mut self,
        transaction_cost: &TransactionCost,
        writable_accounts: impl Iterator<Item = &'a Pubkey> + Clone,
    ) -> Result<UpdatedCosts, CostTrackerError> {
        let cost = transaction_cost.sum();

        if self.block_cost().saturating_add(cost) > self.limits.block_cost {
            // check against the total package cost
            return Err(CostTrackerError::WouldExceedBlockMaxLimit);
        }

        // check if the transaction itself is more costly than the account_cost_limit
        if cost > self.limits.account_cost {
            return Err(CostTrackerError::WouldExceedAccountMaxLimit);
        }

        let allocated_accounts_data_size = self
            .allocated_accounts_data_size
            .load()
            .saturating_add(transaction_cost.allocated_accounts_data_size());

        if allocated_accounts_data_size > self.limits.allocated_data_size {
            return Err(CostTrackerError::WouldExceedAccountDataBlockLimit);
        }

        // Check each account against account_cost_limit and apply the cost in
        // the same lookup. On failure, undo the applied prefix.
        let mut updated_costliest_account_cost = 0;
        for (index, account_key) in writable_accounts.clone().enumerate() {
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
                    Some(cost)
                }
            };
            let Some(new_account_cost) = new_account_cost else {
                // the first `index` accounts were applied before this failure
                self.roll_back_applied_costs(writable_accounts, cost, index);
                return Err(CostTrackerError::WouldExceedAccountMaxLimit);
            };
            updated_costliest_account_cost = updated_costliest_account_cost.max(new_account_cost);
        }

        // every check passed: publish the block-level state
        self.allocated_accounts_data_size
            .store(allocated_accounts_data_size);
        self.transaction_count += 1;
        self.block_cost.fetch_add(cost);

        Ok(UpdatedCosts {
            updated_block_cost: self.block_cost(),
            updated_costliest_account_cost,
        })
    }

    pub fn update_cost<'a>(
        &mut self,
        old_cost_units: u64,
        new_cost_units: u64,
        writable_accounts: impl Iterator<Item = &'a Pubkey>,
    ) {
        match new_cost_units.cmp(&old_cost_units) {
            std::cmp::Ordering::Equal => (),
            std::cmp::Ordering::Greater => {
                self.add_cost(writable_accounts, new_cost_units - old_cost_units);
            }
            std::cmp::Ordering::Less => {
                self.sub_cost(writable_accounts, old_cost_units - new_cost_units);
            }
        }
    }

    /// Undoes the first `num_applied` per account cost applications of a
    /// partially applied transaction by subtracting the cost each one added.
    /// Entries left with zero cost are removed.
    fn roll_back_applied_costs<'a>(
        &mut self,
        writable_accounts: impl Iterator<Item = &'a Pubkey>,
        cost: u64,
        num_applied: usize,
    ) {
        for account_key in writable_accounts.take(num_applied) {
            if let Entry::Occupied(mut entry) = self.cost_by_writable_accounts.entry(*account_key) {
                let new_account_cost = entry.get().saturating_sub(cost);
                if new_account_cost == 0 {
                    entry.remove();
                } else {
                    *entry.get_mut() = new_account_cost;
                }
            }
        }
    }

    pub fn remove<'a>(
        &mut self,
        transaction_cost: &TransactionCost,
        writable_accounts: impl Iterator<Item = &'a Pubkey>,
    ) {
        self.sub_cost(writable_accounts, transaction_cost.sum());
        self.allocated_accounts_data_size.store(
            self.allocated_accounts_data_size
                .load()
                .saturating_sub(transaction_cost.allocated_accounts_data_size()),
        );
        self.transaction_count -= 1;
    }

    pub fn block_cost(&self) -> u64 {
        self.block_cost.load()
    }

    pub fn shared_block_cost(&self) -> SharedBlockCost {
        self.block_cost.clone()
    }

    pub fn shared_allocated_accounts_data_size(&self) -> SharedAllocatedAccountsDataSize {
        self.allocated_accounts_data_size.clone()
    }

    pub fn transaction_count(&self) -> u64 {
        self.transaction_count.0
    }

    pub fn stats(&self) -> CostTrackerStats {
        let (costliest_account, costliest_account_cost) = self.find_costliest_account();
        CostTrackerStats {
            block_cost: self.block_cost(),
            transaction_count: self.transaction_count.0,
            number_of_accounts: self.number_of_accounts(),
            costliest_account,
            costliest_account_cost,
            allocated_accounts_data_size: self.allocated_accounts_data_size.load(),
            in_flight_transaction_count: self.in_flight_transaction_count.0,
            number_of_contended_accounts: self.find_number_of_contended_accounts(),
        }
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

    fn add_cost<'a>(
        &mut self,
        writable_accounts: impl Iterator<Item = &'a Pubkey>,
        adjustment: u64,
    ) {
        for account_key in writable_accounts {
            let account_cost = self
                .cost_by_writable_accounts
                .entry(*account_key)
                .or_insert(0);
            *account_cost = account_cost.saturating_add(adjustment);
        }
        self.block_cost.fetch_add(adjustment);
    }

    fn sub_cost<'a>(
        &mut self,
        writable_accounts: impl Iterator<Item = &'a Pubkey>,
        adjustment: u64,
    ) {
        for account_key in writable_accounts {
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

/// Wrapper around the allocated accounts data size to allow fast sharing of the value without
/// locking. Value is read-only outside of cost-tracker.
#[derive(Debug, Clone)]
pub struct SharedAllocatedAccountsDataSize(Arc<AtomicU64>);

impl SharedAllocatedAccountsDataSize {
    pub fn new(value: u64) -> Self {
        Self(Arc::new(AtomicU64::new(value)))
    }

    fn store(&self, value: u64) {
        self.0.store(value, Ordering::Release);
    }

    pub fn load(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::cmp};

    fn test_cost(cost_units: u64) -> TransactionCost {
        TransactionCost {
            signature_cost: 0,
            write_lock_cost: 0,
            data_bytes_cost: 0,
            programs_execution_cost: cost_units,
            loaded_accounts_data_size_cost: 0,
            allocated_accounts_data_size: 0,
        }
    }

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

    fn test_setup() -> Pubkey {
        agave_logger::setup();
        Pubkey::new_unique()
    }

    fn vote_cost() -> u64 {
        1 + 2 + solana_vote_program::vote_processor::DEFAULT_COMPUTE_UNITS + 8
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
        let accts = [mint_keypair];
        let cost = 5;

        // build testee to have capacity for one cost
        let mut testee = CostTracker::new(cost, cost);
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        assert_eq!(cost, testee.block_cost());
        let (_costliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(cost, costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_ok_add_one_vote() {
        let mint_keypair = test_setup();
        let accts = [mint_keypair];
        let cost = vote_cost();

        // build testee to have capacity for one cost
        let mut testee = CostTracker::new(cost, cost);
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        assert_eq!(cost, testee.block_cost());
        let (_costliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(cost, costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_add_data() {
        let mint_keypair = test_setup();
        let accts = [mint_keypair];
        let cost = 5;
        let transaction_cost = TransactionCost {
            allocated_accounts_data_size: 1,
            ..test_cost(cost)
        };

        // build testee to have capacity for one cost
        let mut testee = CostTracker::new(cost, cost);
        let shared_allocated_accounts_data_size = testee.shared_allocated_accounts_data_size();
        let old = shared_allocated_accounts_data_size.load();
        assert!(testee.try_add(&transaction_cost, accts.iter()).is_ok());
        assert_eq!(old + 1, shared_allocated_accounts_data_size.load());
    }

    #[test]
    fn test_cost_tracker_ok_add_two_same_accounts() {
        let mint_keypair = test_setup();
        // use the same writable account for both costs
        let accts1 = [mint_keypair];
        let cost1 = 5;
        let accts2 = [mint_keypair];
        let cost2 = 5;

        // build testee to have capacity for both costs on the same account
        let mut testee = CostTracker::new(cost1 + cost2, cost1 + cost2);
        {
            assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_ok());
        }
        {
            assert!(testee.try_add(&test_cost(cost2), accts2.iter()).is_ok());
        }
        assert_eq!(cost1 + cost2, testee.block_cost());
        assert_eq!(1, testee.cost_by_writable_accounts.len());
        let (_ccostliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(cost1 + cost2, costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_ok_add_two_diff_accounts() {
        let mint_keypair = test_setup();
        // use a different writable account for each cost
        let second_account = Pubkey::new_unique();
        let accts1 = [mint_keypair];
        let cost1 = 5;

        let accts2 = [second_account];
        let cost2 = 5;

        // build testee to have capacity for both costs
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2);
        {
            assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_ok());
        }
        {
            assert!(testee.try_add(&test_cost(cost2), accts2.iter()).is_ok());
        }
        assert_eq!(cost1 + cost2, testee.block_cost());
        assert_eq!(2, testee.cost_by_writable_accounts.len());
        let (_ccostliest_account, costliest_account_cost) = testee.find_costliest_account();
        assert_eq!(std::cmp::max(cost1, cost2), costliest_account_cost);
    }

    #[test]
    fn test_cost_tracker_chain_reach_limit() {
        let mint_keypair = test_setup();
        // use the same writable account for both costs
        let accts1 = [mint_keypair];
        let cost1 = 5;
        let accts2 = [mint_keypair];
        let cost2 = 5;

        // build testee to have block capacity for both costs, but account capacity for only one
        let mut testee = CostTracker::new(cmp::min(cost1, cost2), cost1 + cost2);
        // should have room for the first cost
        {
            assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_ok());
        }
        // but no more sapce on the same chain (same signer account)
        {
            assert!(testee.try_add(&test_cost(cost2), accts2.iter()).is_err());
        }
    }

    #[test]
    fn test_cost_tracker_reach_limit() {
        let mint_keypair = test_setup();
        // use a different writable account for each cost
        let second_account = Pubkey::new_unique();
        let accts1 = [mint_keypair];
        let cost1 = 5;
        let accts2 = [second_account];
        let cost2 = 5;

        // build testee with account capacity for each cost, but insufficient block capacity for both
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2 - 1);
        // should have room for the first cost
        {
            assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_ok());
        }
        // but no more room for package as whole
        {
            assert!(testee.try_add(&test_cost(cost2), accts2.iter()).is_err());
        }
    }

    #[test]
    fn test_cost_tracker_vote_transactions_use_regular_limits() {
        let mint_keypair = test_setup();
        // use a different writable account for each vote cost
        let second_account = Pubkey::new_unique();
        let accts1 = [mint_keypair];
        let cost1 = vote_cost();
        let accts2 = [second_account];
        let cost2 = vote_cost();

        // build testee to have capacity for both vote costs
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2);
        // should have room for first vote
        {
            assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_ok());
        }
        assert!(testee.try_add(&test_cost(cost2), accts2.iter()).is_ok());
    }

    #[test]
    fn test_cost_tracker_reach_data_block_limit() {
        let mint_keypair = test_setup();
        // use a different writable account for each cost
        let second_account = Pubkey::new_unique();
        let accts1 = [mint_keypair];
        let accts2 = [second_account];
        let cost1 = 5;
        let cost2 = 5;

        // build testee that passes
        let mut testee = CostTracker::new(cmp::max(cost1, cost2), cost1 + cost2);
        assert!(
            testee
                .try_add(
                    &TransactionCost {
                        allocated_accounts_data_size: MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA,
                        ..test_cost(cost1)
                    },
                    accts1.iter(),
                )
                .is_ok()
        );
        // data is too big
        assert!(matches!(
            testee.try_add(
                &TransactionCost {
                    allocated_accounts_data_size: MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA + 1,
                    ..test_cost(cost2)
                },
                accts2.iter(),
            ),
            Err(CostTrackerError::WouldExceedAccountDataBlockLimit),
        ));
    }

    #[test]
    fn test_cost_tracker_respects_custom_allocated_data_size_limit() {
        // Set up a cost that allocates 2 bytes.
        let mint_keypair = test_setup();
        let accts = [mint_keypair];

        // Transaction fits with default limit.
        let mut testee = CostTracker::new(u64::MAX, u64::MAX);
        assert!(
            testee
                .try_add(
                    &TransactionCost {
                        allocated_accounts_data_size: 2,
                        ..test_cost(5)
                    },
                    accts.iter(),
                )
                .is_ok()
        );

        // Transaction does not fit with 1B limit.
        testee.set_limits(CostTrackerLimits {
            allocated_data_size: 1,
            ..testee.get_limits()
        });
        assert!(matches!(
            testee.try_add(
                &TransactionCost {
                    allocated_accounts_data_size: 2,
                    ..test_cost(5)
                },
                accts.iter(),
            ),
            Err(CostTrackerError::WouldExceedAccountDataBlockLimit),
        ));
    }

    #[test]
    fn test_cost_tracker_remove() {
        let mint_keypair = test_setup();
        // use a different writable account for each cost
        let second_account = Pubkey::new_unique();
        let accts1 = [mint_keypair];
        let accts2 = [second_account];
        let cost1 = 5;
        let cost2 = 5;

        // build testee
        let mut testee = CostTracker::new(cost1 + cost2, cost1 + cost2);

        assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_ok());
        assert!(testee.try_add(&test_cost(cost2), accts2.iter()).is_ok());
        assert_eq!(cost1 + cost2, testee.block_cost());

        // removing a cost affects block_cost
        testee.remove(&test_cost(cost1), accts1.iter());
        assert_eq!(cost2, testee.block_cost());

        // add back the first cost
        assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_ok());
        assert_eq!(cost1 + cost2, testee.block_cost());

        // cannot add the first cost again, cost limit would be exceeded
        assert!(testee.try_add(&test_cost(cost1), accts1.iter()).is_err());
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

        // case 1: apply the cost to 3 accounts; we will have:
        // | acct1 | $cost |
        // | acct2 | $cost |
        // | acct3 | $cost |
        // and block_cost = $cost
        {
            let accts = [acct1, acct2, acct3];
            assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
            let (_costliest_account, costliest_account_cost) = testee.find_costliest_account();
            assert_eq!(cost, testee.block_cost());
            assert_eq!(3, testee.cost_by_writable_accounts.len());
            assert_eq!(cost, costliest_account_cost);
        }

        // case 2: apply the cost to acct2, resulting in:
        // | acct1 | $cost |
        // | acct2 | $cost * 2 |
        // | acct3 | $cost |
        // and block_cost = $cost * 2
        {
            let accts = [acct2];
            assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
            let (costliest_account, costliest_account_cost) = testee.find_costliest_account();
            assert_eq!(cost * 2, testee.block_cost());
            assert_eq!(3, testee.cost_by_writable_accounts.len());
            assert_eq!(cost * 2, costliest_account_cost);
            assert_eq!(acct2, costliest_account);
        }

        // case 3: apply the cost to [acct1, acct2]; acct2 exceeds the limit, so this fails atomically,
        // we should still have:
        // | acct1 | $cost |
        // | acct2 | $cost * 2 |
        // | acct3 | $cost |
        // and block_cost = $cost * 2
        {
            let accts = [acct1, acct2];
            assert!(testee.try_add(&test_cost(cost), accts.iter()).is_err());
            let (costliest_account, costliest_account_cost) = testee.find_costliest_account();
            assert_eq!(cost * 2, testee.block_cost());
            assert_eq!(3, testee.cost_by_writable_accounts.len());
            assert_eq!(cost * 2, costliest_account_cost);
            assert_eq!(acct2, costliest_account);
            // the pre-existing acct1 entry was decremented back, not removed
            assert_eq!(Some(&cost), testee.cost_by_writable_accounts.get(&acct1));
        }

        // case 4: apply the cost to [acct4 (unseen), acct2]; acct2 exceeds the limit,
        // the entry freshly inserted for acct4 must be removed by the rollback,
        // leaving the tracker exactly as after case 2
        {
            let acct4 = Pubkey::new_unique();
            let accts = [acct4, acct2];
            assert!(matches!(
                testee.try_add(&test_cost(cost), accts.iter()),
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
    fn test_try_add_rollback_many_accounts() {
        let cost = 100;
        let hot_account = Pubkey::new_unique();
        let mut testee = CostTracker::new(cost * 2, cost * 1000);

        // drive hot_account to the limit so the next charge fails
        let accts = [hot_account];
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        let block_cost_before = testee.block_cost();

        // 100 fresh accounts followed by hot_account, all 100 fresh entries
        // are inserted before the failure at index 100
        let mut keys: Vec<Pubkey> = (0..100).map(|_| Pubkey::new_unique()).collect();
        keys.push(hot_account);
        let accts = keys;
        assert!(matches!(
            testee.try_add(&test_cost(cost), accts.iter()),
            Err(CostTrackerError::WouldExceedAccountMaxLimit)
        ));

        assert_eq!(1, testee.cost_by_writable_accounts.len());
        assert_eq!(
            Some(&(cost * 2)),
            testee.cost_by_writable_accounts.get(&hot_account)
        );
        assert_eq!(block_cost_before, testee.block_cost());
    }

    // Duplicate writable keys net out.
    // Each occurrence's undo subtracts what it added, and the entry is removed when it reaches zero
    #[test]
    fn test_try_add_rollback_with_duplicate_keys() {
        let cost = 100;
        let dup = Pubkey::new_unique();
        let hot_account = Pubkey::new_unique();
        let mut testee = CostTracker::new(cost * 4, cost * 1000);

        // drive hot_account to the limit so any further charge fails
        let accts = [hot_account];
        for _ in 0..4 {
            assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        }
        let block_cost_before = testee.block_cost();

        // fresh dup - each undo subtracts what that occurrence added; the entry reaches zero on the second undo and is removed
        let accts = [dup, dup, hot_account];
        assert!(matches!(
            testee.try_add(&test_cost(cost), accts.iter()),
            Err(CostTrackerError::WouldExceedAccountMaxLimit)
        ));
        assert!(!testee.cost_by_writable_accounts.contains_key(&dup));
        assert_eq!(block_cost_before, testee.block_cost());

        // pre-existing dup
        let accts = [dup];
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        let block_cost_before = testee.block_cost();

        let accts = [dup, dup, hot_account];
        assert!(matches!(
            testee.try_add(&test_cost(cost), accts.iter()),
            Err(CostTrackerError::WouldExceedAccountMaxLimit)
        ));
        assert_eq!(Some(&cost), testee.cost_by_writable_accounts.get(&dup));
        assert_eq!(block_cost_before, testee.block_cost());
    }

    #[test]
    fn test_try_add_rollback_removes_zeroed_entries() {
        let cost = 100;
        let zeroed = Pubkey::new_unique();
        let hot_account = Pubkey::new_unique();
        let mut testee = CostTracker::new(cost * 2, cost * 1000);

        // leave `zeroed` in the map with zero cost
        let accts = [zeroed];
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        testee.remove(&test_cost(cost), accts.iter());
        assert_eq!(Some(&0), testee.cost_by_writable_accounts.get(&zeroed));

        // drive hot_account to the limit so the next charge fails
        let accts = [hot_account];
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        let block_cost_before = testee.block_cost();

        let accts = [zeroed, hot_account];
        assert!(matches!(
            testee.try_add(&test_cost(cost), accts.iter()),
            Err(CostTrackerError::WouldExceedAccountMaxLimit)
        ));
        assert!(!testee.cost_by_writable_accounts.contains_key(&zeroed));
        assert_eq!(
            Some(&(cost * 2)),
            testee.cost_by_writable_accounts.get(&hot_account)
        );
        assert_eq!(block_cost_before, testee.block_cost());
    }

    #[test]
    fn test_adjust_cost() {
        let acct1 = Pubkey::new_unique();
        let acct2 = Pubkey::new_unique();
        let acct3 = Pubkey::new_unique();
        let cost = 100;
        let account_max = cost * 2;
        let block_max = account_max * 3; // for three accts

        let mut testee = CostTracker::new(account_max, block_max);
        let accts = [acct1, acct2, acct3];
        let mut expected_block_cost = cost;
        let expected_tx_count = 1;
        assert!(testee.try_add(&test_cost(cost), accts.iter()).is_ok());
        assert_eq!(expected_block_cost, testee.block_cost());
        assert_eq!(expected_tx_count, testee.transaction_count());
        testee
            .cost_by_writable_accounts
            .iter()
            .for_each(|(_key, units)| {
                assert_eq!(expected_block_cost, *units);
            });

        // adjust down
        {
            let adjustment = 50u64;
            let new_cost = expected_block_cost - adjustment;
            testee.update_cost(expected_block_cost, new_cost, accts.iter());
            expected_block_cost = new_cost;
            assert_eq!(expected_block_cost, testee.block_cost());
            assert_eq!(expected_tx_count, testee.transaction_count());
            testee
                .cost_by_writable_accounts
                .iter()
                .for_each(|(_key, units)| {
                    assert_eq!(expected_block_cost, *units);
                });
        }

        // adjust up
        {
            let new_cost = expected_block_cost + 25;
            testee.update_cost(expected_block_cost, new_cost, accts.iter());
            expected_block_cost = new_cost;
            assert_eq!(expected_block_cost, testee.block_cost());
            assert_eq!(expected_tx_count, testee.transaction_count());
        }

        // no adjustment
        testee.update_cost(expected_block_cost, expected_block_cost, accts.iter());
        assert_eq!(expected_block_cost, testee.block_cost());
    }

    #[test]
    fn test_remove_cost() {
        let mut cost_tracker = CostTracker::default();

        let cost = 100u64;
        let accts = [Pubkey::new_unique()];
        let transaction_cost = || TransactionCost {
            allocated_accounts_data_size: 7,
            ..test_cost(cost)
        };
        cost_tracker
            .try_add(&transaction_cost(), accts.iter())
            .unwrap();
        // assert cost_tracker is reverted to default
        assert_eq!(1, cost_tracker.transaction_count.0);
        assert_eq!(1, cost_tracker.number_of_accounts());
        assert_eq!(cost, cost_tracker.block_cost());
        assert_eq!(7, cost_tracker.allocated_accounts_data_size.load());

        cost_tracker.remove(&transaction_cost(), accts.iter());
        // assert cost_tracker is reverted to default
        assert_eq!(0, cost_tracker.transaction_count.0);
        assert_eq!(0, cost_tracker.number_of_accounts());
        assert_eq!(0, cost_tracker.block_cost());
        assert_eq!(0, cost_tracker.allocated_accounts_data_size.load());
        let stats = cost_tracker.stats();
        assert_eq!(stats.block_cost, 0);
        assert_eq!(stats.transaction_count, 0);
        assert_eq!(stats.number_of_accounts, 0);
        assert_eq!(stats.costliest_account, accts[0]);
        assert_eq!(stats.costliest_account_cost, 0);
        assert_eq!(stats.allocated_accounts_data_size, 0);
        assert_eq!(stats.in_flight_transaction_count, 0);
        assert_eq!(stats.number_of_contended_accounts, 0);
    }

    #[test]
    fn test_cost_tracker_stats() {
        let mint_keypair = test_setup();
        let accts = [mint_keypair];
        let transaction_cost = TransactionCost {
            allocated_accounts_data_size: 7,
            ..test_cost(95)
        };

        let mut cost_tracker = CostTracker::new(100, 1_000);
        cost_tracker
            .try_add(&transaction_cost, accts.iter())
            .unwrap();
        cost_tracker.add_transactions_in_flight(2);

        let stats = cost_tracker.stats();
        assert_eq!(stats.block_cost, 95);
        assert_eq!(stats.transaction_count, 1);
        assert_eq!(stats.number_of_accounts, 1);
        assert_eq!(stats.costliest_account, mint_keypair);
        assert_eq!(stats.costliest_account_cost, 95);
        assert_eq!(stats.allocated_accounts_data_size, 7);
        assert_eq!(stats.in_flight_transaction_count, 2);
        assert_eq!(stats.number_of_contended_accounts, 1);

        cost_tracker.update_cost(95, 90, accts.iter());
        let adjusted_stats = cost_tracker.stats();
        assert_eq!(adjusted_stats.block_cost, 90);
        assert_eq!(adjusted_stats.costliest_account_cost, 90);
        assert_eq!(adjusted_stats.number_of_contended_accounts, 0);
    }

    #[test]
    fn test_get_cost_by_writable_accounts_post_analysis() {
        let mut cost_tracker = CostTracker::default();
        let cost = 100u64;
        let accts = [Pubkey::new_unique()];
        cost_tracker
            .try_add(&test_cost(cost), accts.iter())
            .unwrap();
        let cost_by_writable_accounts = cost_tracker.get_cost_by_writable_accounts();
        assert_eq!(1, cost_by_writable_accounts.len());
        assert_eq!(cost, *cost_by_writable_accounts.values().next().unwrap());
        assert_eq!(
            *cost_by_writable_accounts,
            cost_tracker.cost_by_writable_accounts
        );
    }
}
