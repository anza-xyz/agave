use {
    super::{utils::super_majority_threshold, Stake},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        collections::{BTreeMap, BTreeSet, HashMap},
        fmt::Debug,
        ops::RangeInclusive,
    },
    thiserror::Error,
};

#[derive(Debug, Error, PartialEq)]
pub enum AddVoteError {
    #[error("Skip vote {0:?} already exists")]
    AlreadyExists(RangeInclusive<Slot>),

    #[error("Newer skip vote {0:?} than {1:?} already exists for this pubkey")]
    TooOld(RangeInclusive<Slot>, RangeInclusive<Slot>),

    #[error("Overlapping skip vote old {0:?} and new {1:?}")]
    Overlapping(RangeInclusive<Slot>, RangeInclusive<Slot>),

    #[error("Zero stake")]
    ZeroStake,
}

/// A trait for objects that provide a stake value.
pub trait HasStake {
    fn stake_value(&self) -> Stake;
    fn pubkey(&self) -> Pubkey;
}

/// Implement `HasStake` for `(Pubkey, Stake)`
impl HasStake for (Pubkey, Stake) {
    fn stake_value(&self) -> Stake {
        self.1
    }

    fn pubkey(&self) -> Pubkey {
        self.0
    }
}

/// Dynamic Segment Tree that works with any type implementing `HasStake`
struct DynamicSegmentTree<T: Ord + Clone + HasStake> {
    /// (starts, ends) per `slot`, indicating the items that start and end at `slot`
    tree: BTreeMap<Slot, (Vec<T>, Vec<T>)>,
}

impl<T: Ord + Clone + Debug + HasStake> DynamicSegmentTree<T> {
    /// Initializes an empty dynamic segment tree
    fn new() -> Self {
        Self {
            tree: BTreeMap::new(),
        }
    }

    /// Inserts a given range `[start, end]` with an item `value`
    fn insert(&mut self, start: Slot, end: Slot, new_value: T) {
        self.tree
            .entry(start)
            .or_default()
            .0
            .push(new_value.clone());
        self.tree.entry(end).or_default().1.push(new_value);
    }

    /// Removes a given range `[start, end]` with an item `value`
    fn remove(&mut self, start: Slot, end: Slot, new_value: T) {
        if let Some((starts, _)) = self.tree.get_mut(&start) {
            starts.retain(|v| v.pubkey() != new_value.pubkey());
        }
        if let Some((_, ends)) = self.tree.get_mut(&end) {
            ends.retain(|v| v.pubkey() != new_value.pubkey());
        }
    }

    fn scan_certificates(&self, threshold_stake: f64) -> Vec<((Slot, Slot), BTreeSet<Pubkey>)> {
        let mut accumulated = 0f64;
        let mut current_contributors = BTreeSet::new();
        let mut current_cert: Option<(Slot, BTreeSet<Pubkey>)> = None;
        let mut certs: Vec<((Slot, Slot), BTreeSet<Pubkey>)> = vec![];

        for (slot, (starts, ends)) in self.tree.iter() {
            let mut new_contributors = vec![];

            // Add new stakes
            for item in starts {
                current_contributors.insert(item.pubkey());
                new_contributors.push(item.pubkey());
                accumulated += item.stake_value() as f64;
            }

            // Start or increment current cert
            if accumulated > threshold_stake {
                match &mut current_cert {
                    None => {
                        // Start a cert
                        current_cert = Some((*slot, current_contributors.clone()));
                        // Check if the previous cert is consecutive
                        if let Some(((_, prev_end), _)) = certs.last() {
                            if prev_end + 1 == *slot {
                                // Overwrite the newly started cert with an extension of the previous
                                let ((prev_start, _), mut prev_contributors) = certs
                                    .pop()
                                    .expect("`certs` has at least one element checked above");
                                prev_contributors.extend(current_contributors.clone());
                                current_cert = Some((prev_start, prev_contributors));
                            }
                        }
                    }
                    Some((_, ref mut contributors)) => {
                        // Active cert, still above threshold, add any new contributors as
                        // we want to build the maximal certificate
                        contributors.extend(new_contributors)
                    }
                }
            }

            // Subtract stakes that end on this slot
            for item in ends {
                current_contributors.remove(&item.pubkey());
                accumulated -= item.stake_value() as f64
            }

            // Return cert if it has ended
            if accumulated <= threshold_stake {
                if let Some((start_slot, contributors)) = &current_cert {
                    // Skip certificate has ended, reset and publish
                    certs.push(((*start_slot, *slot), contributors.clone()));
                    current_cert = None;
                }
            }
        }

        debug_assert_eq!(accumulated, 0f64);
        debug_assert!(current_cert.is_none());
        certs
    }
}

/// Structure to store a skip vote, including the range and transaction
pub struct SkipVote<T> {
    skip_range: RangeInclusive<Slot>,
    data: T,
}

/// `SkipPool` tracks validator skip votes and aggregates stake using a dynamic segment tree.
pub struct SkipPool<T: Clone> {
    skips: HashMap<Pubkey, SkipVote<T>>, // Stores latest skip range for each validator
    segment_tree: DynamicSegmentTree<(Pubkey, Stake)>, // Generic tree tracking validators' stake
    /// The current ranges of slots that are skip certified
    certificate_ranges: Vec<RangeInclusive<Slot>>,
    /// Whether `certificate_ranges` is up to date
    up_to_date: bool,
}

impl<T: Clone> Default for SkipPool<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> SkipPool<T> {
    /// Initializes the `SkipPool`
    pub fn new() -> Self {
        Self {
            skips: HashMap::new(),
            segment_tree: DynamicSegmentTree::new(),
            certificate_ranges: Vec::default(),
            up_to_date: true,
        }
    }

    /// Adds a skip vote for a validator and updates the segment tree
    pub fn add_vote(
        &mut self,
        pubkey: &Pubkey,
        skip_range: RangeInclusive<Slot>,
        data: T,
        stake: Stake,
    ) -> Result<(), AddVoteError> {
        if stake == 0 {
            return Err(AddVoteError::ZeroStake);
        }
        // Remove previous skip vote if it exists
        if let Some(prev_skip_vote) = self.skips.get(pubkey) {
            if prev_skip_vote.skip_range == skip_range {
                return Err(AddVoteError::AlreadyExists(
                    prev_skip_vote.skip_range.clone(),
                ));
            }
            if prev_skip_vote.skip_range.end() >= skip_range.end() {
                return Err(AddVoteError::TooOld(
                    prev_skip_vote.skip_range.clone(),
                    skip_range,
                ));
            }

            // Extensions are allowed, i.e. (1..=3) to (1..=5)
            if skip_range.start() == prev_skip_vote.skip_range.start() {
                // Guaranteed by above TooOld check
                assert!(skip_range.end() > prev_skip_vote.skip_range.end());
            } else if skip_range.start() <= prev_skip_vote.skip_range.end() {
                return Err(AddVoteError::Overlapping(
                    prev_skip_vote.skip_range.clone(),
                    skip_range,
                ));
            }

            self.segment_tree.remove(
                *prev_skip_vote.skip_range.start(),
                *prev_skip_vote.skip_range.end(),
                (*pubkey, (stake as Stake)), // stake doesn't actually matter here
            );
        }

        // Add new skip range
        self.segment_tree.insert(
            *skip_range.start(),
            *skip_range.end(),
            (*pubkey, stake as Stake), // Add stake
        );

        // Store the validator's updated skip vote
        self.skips.insert(
            *pubkey,
            SkipVote {
                skip_range: skip_range.clone(),
                data,
            },
        );

        self.up_to_date = false;
        if self.skip_range_certified(skip_range.start(), skip_range.end()) {
            // The vote is already contained in a cert, not necessary to update
            self.up_to_date = true;
        }

        Ok(())
    }

    pub fn max_skip_certificate_range(&self) -> &RangeInclusive<Slot> {
        self.certificate_ranges.last().unwrap_or(&(0..=0))
    }

    /// Get all skip certificates
    pub fn get_skip_certificates(&self, total_stake: Stake) -> Vec<(RangeInclusive<Slot>, Vec<T>)> {
        let threshold = super_majority_threshold(total_stake);
        self.segment_tree
            .scan_certificates(threshold)
            .into_iter()
            .map(|((start, end), contributors)| {
                (
                    start..=end,
                    contributors
                        .iter()
                        .filter_map(|pk| self.skips.get(pk))
                        .map(|sv| sv.data.clone())
                        .collect(),
                )
            })
            .collect()
    }

    pub fn update(&mut self, total_stake: Stake) {
        let threshold = super_majority_threshold(total_stake);
        self.certificate_ranges = self
            .segment_tree
            .scan_certificates(threshold)
            .into_iter()
            .map(|((start, end), _)| start..=end)
            .collect();
        self.up_to_date = true;
    }

    /// Is `slot` contained in any skip certificates
    pub fn skip_certified(&mut self, slot: Slot, total_stake: Stake) -> bool {
        if self
            .certificate_ranges
            .iter()
            .any(|range| range.contains(&slot))
        {
            // If we are already have a certificate no reason to rescan (potentially costly)
            return true;
        }

        if !self.up_to_date {
            // No certificate is found and ranges are out of date, rescan and retry
            self.update(total_stake);
            return self.skip_certified(slot, total_stake);
        }

        false
    }

    /// Is the given slot range contained in any skip certificates
    pub fn skip_range_certified(&self, start_slot: &Slot, end_slot: &Slot) -> bool {
        self.certificate_ranges
            .iter()
            .any(|range| range.contains(start_slot) && range.contains(end_slot))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_transaction::versioned::VersionedTransaction};

    fn dummy_transaction() -> VersionedTransaction {
        VersionedTransaction::default() // Creates a dummy transaction for testing
    }

    fn assert_single_certificate_range<T: Clone>(
        pool: &SkipPool<T>,
        total_stake: Stake,
        exp_range: RangeInclusive<Slot>,
    ) {
        let [(ref range, _)] = pool.get_skip_certificates(total_stake)[..] else {
            panic!("skip cert failure");
        };
        assert_eq!(*range, exp_range);
    }

    #[test]
    fn test_add_single_vote() {
        let mut pool = SkipPool::new();
        let validator = Pubkey::new_unique();
        let skip_range = 10..=20;
        let skip_tx = dummy_transaction();
        let stake = 70;
        let total_stake = 100;

        pool.add_vote(&validator, skip_range.clone(), skip_tx.clone(), stake)
            .unwrap();

        let stored_vote = pool.skips.get(&validator).unwrap();
        assert_eq!(stored_vote.skip_range, skip_range);
        assert_eq!(stored_vote.data, skip_tx);
        assert_single_certificate_range(&pool, total_stake, 10..=20);
    }

    #[test]
    fn test_add_vote_zero_stake() {
        let mut pool = SkipPool::new();
        let validator = Pubkey::new_unique();
        let skip_range = 1..=1;
        let skip_tx = dummy_transaction();
        let stake = 0;

        assert_eq!(
            pool.add_vote(&validator, skip_range.clone(), skip_tx.clone(), stake,),
            Err(AddVoteError::ZeroStake)
        );
    }

    #[test]
    fn test_add_singleton_range() {
        let mut pool = SkipPool::new();
        let validator = Pubkey::new_unique();
        let skip_range = 1..=1;
        let skip_tx = dummy_transaction();
        let stake = 70;
        let total_stake = 100;

        pool.add_vote(&validator, skip_range.clone(), skip_tx.clone(), stake)
            .unwrap();

        let stored_vote = pool.skips.get(&validator).unwrap();
        assert_eq!(stored_vote.skip_range, skip_range);
        assert_eq!(stored_vote.data, skip_tx);
        assert_single_certificate_range(&pool, total_stake, 1..=1);
    }

    #[test]
    fn test_consecutive_slots() -> Result<(), AddVoteError> {
        let mut pool = SkipPool::new();
        let total_stake = 100;
        let validator1 = Pubkey::new_unique();
        let single_slot_skippers = [Pubkey::new_unique(); 10];

        pool.add_vote(&validator1, 5..=15, dummy_transaction(), 75)?;

        for (i, validator) in single_slot_skippers.into_iter().enumerate() {
            let slot = i as u64 + 16;
            // These should not extend the skip range
            pool.add_vote(&validator, slot..=slot, dummy_transaction(), 1)?;
        }

        assert_single_certificate_range(&pool, total_stake, 5..=15);
        Ok(())
    }

    #[test]
    fn test_contributer_removed() -> Result<(), AddVoteError> {
        let mut pool = SkipPool::new();
        let total_stake = 100;
        let small_non_contributor = Pubkey::new_unique();
        let validator = Pubkey::new_unique();

        pool.add_vote(&small_non_contributor, 5..=5, dummy_transaction(), 1)?;
        pool.add_vote(&validator, 6..=10, dummy_transaction(), 75)?;

        let [(ref range, ref contributors)] = pool.get_skip_certificates(total_stake)[..] else {
            panic!("skip cert failure");
        };
        assert_eq!(*range, RangeInclusive::new(6, 10));
        assert_eq!(contributors.len(), 1);
        Ok(())
    }

    #[test]
    fn test_multi_cert() -> Result<(), AddVoteError> {
        let mut pool = SkipPool::new();
        let total_stake = 100;
        let validator1 = Pubkey::new_unique();
        let validator2 = Pubkey::new_unique();
        let validator3 = Pubkey::new_unique();

        pool.add_vote(&validator1, 5..=15, dummy_transaction(), 66)?;
        pool.add_vote(&validator2, 5..=8, dummy_transaction(), 1)?;
        pool.add_vote(&validator3, 11..=15, dummy_transaction(), 1)?;

        let certificates = pool.get_skip_certificates(total_stake);
        assert_eq!(certificates.len(), 2);
        assert_eq!(certificates[0].0, RangeInclusive::new(5, 8));
        assert_eq!(certificates[1].0, RangeInclusive::new(11, 15));
        assert!(pool.skip_certified(6, total_stake));
        assert!(pool.skip_certified(12, total_stake));

        Ok(())
    }

    #[test]
    fn test_add_multiple_votes() {
        let mut pool = SkipPool::new();
        let validator1 = Pubkey::new_unique();
        let validator2 = Pubkey::new_unique();
        let total_stake = 100;

        pool.add_vote(&validator1, 5..=15, dummy_transaction(), 50)
            .unwrap();
        pool.add_vote(&validator2, 20..=30, dummy_transaction(), 50)
            .unwrap();
        assert!(pool.get_skip_certificates(total_stake).is_empty());

        pool.add_vote(&validator1, 5..=30, dummy_transaction(), 50)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 20..=30);
    }

    #[test]
    fn test_add_multiple_disjoint_votes() {
        let mut pool = SkipPool::new();
        let validator1 = Pubkey::new_unique();
        let validator2 = Pubkey::new_unique();
        let validator3 = Pubkey::new_unique();
        let validator4 = Pubkey::new_unique();
        let total_stake = 100;

        pool.add_vote(&validator1, 1..=10, dummy_transaction(), 66)
            .unwrap();

        pool.add_vote(&validator2, 2..=2, dummy_transaction(), 1)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 2..=2);

        pool.add_vote(&validator3, 4..=4, dummy_transaction(), 1)
            .unwrap();
        let certificates = pool.get_skip_certificates(total_stake);
        assert_eq!(certificates.len(), 2);
        assert_eq!(certificates[0].0, 2..=2);
        assert_eq!(certificates[1].0, 4..=4);

        pool.add_vote(&validator4, 3..=3, dummy_transaction(), 1)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 2..=4);
        assert!(pool.skip_certified(3, total_stake));

        pool.add_vote(&validator4, 3..=10, dummy_transaction(), 1)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 2..=10);
        assert!(pool.skip_certified(7, total_stake));
    }

    #[test]
    fn test_two_validators_overlapping_votes() {
        let mut pool = SkipPool::new();
        let validator1 = Pubkey::new_unique();
        let validator2 = Pubkey::new_unique();
        let total_stake = 100;

        let tx1 = dummy_transaction();
        let tx2 = dummy_transaction();

        pool.add_vote(&validator1, 10..=20, tx1.clone(), 50)
            .unwrap();
        pool.add_vote(&validator2, 15..=25, tx2.clone(), 50)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 15..=20);

        // Test certificate is correct
        let [(ref range, ref transactions)] = pool.get_skip_certificates(total_stake)[..] else {
            panic!("skip cert failure");
        };
        assert_eq!(*range, 15..=20);
        assert_eq!(transactions.len(), 2);
        assert!(transactions.contains(&tx1));
        assert!(transactions.contains(&tx2));
    }

    #[test]
    fn test_update_existing_singleton_vote() {
        let mut pool = SkipPool::new();
        let validator = Pubkey::new_unique();
        let total_stake = 100;
        // Range expansion on a singleton vote should be ok
        assert!(pool
            .add_vote(&validator, 1..=1, dummy_transaction(), 70)
            .is_ok());
        pool.add_vote(&validator, 1..=6, dummy_transaction(), 70)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 1..=6);
    }

    #[test]
    fn test_update_existing_vote() {
        let mut pool = SkipPool::new();
        let validator = Pubkey::new_unique();
        let total_stake = 100;

        pool.add_vote(&validator, 10..=20, dummy_transaction(), 70)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 10..=20);

        // AlreadyExists failure
        assert_eq!(
            pool.add_vote(&validator, 10..=20, dummy_transaction(), 70),
            Err(AddVoteError::AlreadyExists(10..=20))
        );

        // TooOld failure (trying to add 15..=17 when 10..=20 already exists)
        assert_eq!(
            pool.add_vote(&validator, 15..=17, dummy_transaction(), 70),
            Err(AddVoteError::TooOld(10..=20, 15..=17))
        );

        // TooOld falure with same range start but smaller range end
        assert_eq!(
            pool.add_vote(&validator, 10..=19, dummy_transaction(), 70),
            Err(AddVoteError::TooOld(10..=20, 10..=19))
        );

        // Overlapping failures
        assert_eq!(
            pool.add_vote(&validator, 15..=25, dummy_transaction(), 70),
            Err(AddVoteError::Overlapping(10..=20, 15..=25))
        );

        assert_eq!(
            pool.add_vote(&validator, 20..=25, dummy_transaction(), 70),
            Err(AddVoteError::Overlapping(10..=20, 20..=25))
        );

        // Adding a new, non-overlapping range
        pool.add_vote(&validator, 21..=22, dummy_transaction(), 70)
            .unwrap();

        // Range extension is allowed
        pool.add_vote(&validator, 21..=23, dummy_transaction(), 70)
            .unwrap();
        assert_single_certificate_range(&pool, total_stake, 21..=23);
    }

    #[test]
    fn test_threshold_not_reached() {
        let mut pool = SkipPool::new();
        let validator1 = Pubkey::new_unique();
        let validator2 = Pubkey::new_unique();

        pool.add_vote(&validator1, 5..=15, dummy_transaction(), 30)
            .unwrap();
        pool.add_vote(&validator2, 20..=30, dummy_transaction(), 30)
            .unwrap();
    }

    #[test]
    fn test_update_and_skip_range_certify() {
        let mut pool = SkipPool::new();
        let validator1 = Pubkey::new_unique();
        let validator2 = Pubkey::new_unique();
        let total_stake = 100;

        pool.add_vote(&validator1, 5..=15, dummy_transaction(), 50)
            .unwrap();
        pool.add_vote(&validator2, 10..=30, dummy_transaction(), 50)
            .unwrap();
        pool.update(total_stake);
        assert!(!pool.skip_range_certified(&5, &10));
        assert!(pool.skip_range_certified(&10, &15));
        assert!(pool.skip_range_certified(&11, &12));
        assert!(!pool.skip_range_certified(&15, &30));
    }
}
