use {
    crate::bank_id::BankId,
    core::fmt::{Debug, Formatter},
    solana_clock::Slot,
};

#[derive(Clone, Default, PartialEq)]
pub struct Ancestors {
    /// Sorted by slot, with at most one bank per slot
    ancestors: Vec<(Slot, BankId)>,
}

impl Debug for Ancestors {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.keys())
    }
}

/// Pairs each slot with a bank id equal to it
impl From<Vec<Slot>> for Ancestors {
    fn from(source: Vec<Slot>) -> Ancestors {
        Ancestors::from(
            source
                .into_iter()
                .map(|slot| (slot, BankId::new(slot)))
                .collect::<Vec<_>>(),
        )
    }
}

impl From<Vec<(Slot, BankId)>> for Ancestors {
    fn from(mut source: Vec<(Slot, BankId)>) -> Ancestors {
        source.sort_unstable_by_key(|(slot, _bank_id)| *slot);
        debug_assert!(
            source.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "ancestors cannot contain duplicate slots: {source:?}"
        );
        Ancestors { ancestors: source }
    }
}

impl Ancestors {
    pub fn keys(&self) -> Vec<Slot> {
        self.iter().collect()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Slot> + '_ {
        self.ancestors.iter().map(|(slot, _bank_id)| *slot)
    }

    /// Removes the entry with the highest slot, returning that slot
    pub fn remove_max_slot(&mut self) -> Option<Slot> {
        self.ancestors.pop().map(|(slot, _bank_id)| slot)
    }

    pub fn contains_key(&self, slot: &Slot) -> bool {
        self.ancestors
            .binary_search_by_key(slot, |(slot, _bank_id)| *slot)
            .is_ok()
    }

    pub fn len(&self) -> usize {
        self.ancestors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ancestors.is_empty()
    }

    pub fn min_slot(&self) -> Option<Slot> {
        self.ancestors.first().map(|(slot, _bank_id)| *slot)
    }

    pub fn max_slot(&self) -> Slot {
        self.ancestors.last().map_or(0, |(slot, _bank_id)| *slot)
    }

    /// Is an index entry at `slot` an ancestor?
    /// This includes any ancestors and any slots older than the oldest ancestor in the list
    pub fn is_ancestor(&self, slot: Slot) -> bool {
        self.min_slot().is_none_or(|min_slot| slot <= min_slot) || self.contains_key(&slot)
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl Ancestors {
    /// Pairs `slot` with a bank id equal to it
    pub fn insert(&mut self, slot: Slot) {
        if let Err(index) = self
            .ancestors
            .binary_search_by_key(&slot, |(slot, _bank_id)| *slot)
        {
            self.ancestors.insert(index, (slot, BankId::new(slot)));
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, log::*, solana_measure::measure::Measure, std::collections::HashSet};

    #[test]
    fn test_ancestors_permutations() {
        let mut ancestors = Ancestors::default();
        let mut hash = HashSet::new();

        let min = 101_000;
        let width = 400_000;
        let dead = 19;

        let mut slot = min;
        while hash.len() < width {
            slot += 1;
            if slot % dead == 0 {
                continue;
            }
            hash.insert(slot);
            ancestors.insert(slot);
        }
        compare_ancestors(&hash, &ancestors);

        let max = slot + 1;

        let mut time = Measure::start("");
        let mut count = 0;
        for slot in (min - 10)..max + 100 {
            if hash.contains(&slot) {
                count += 1;
            }
        }
        time.stop();

        let mut time2 = Measure::start("");
        let mut count2 = 0;
        for slot in (min - 10)..max + 100 {
            if ancestors.contains_key(&slot) {
                count2 += 1;
            }
        }
        time2.stop();
        info!(
            "{}ms, {}ms, {} ratio",
            time.as_ms(),
            time2.as_ms(),
            time.as_ns() / time2.as_ns()
        );
        assert_eq!(count, count2);
    }

    fn compare_ancestors(hashset: &HashSet<u64>, ancestors: &Ancestors) {
        assert_eq!(hashset.len(), ancestors.len());
        assert_eq!(hashset.is_empty(), ancestors.is_empty());
        let mut min = u64::MAX;
        let mut max = 0;
        for item in hashset.iter() {
            let key = item;
            min = std::cmp::min(min, *key);
            max = std::cmp::max(max, *key);
            assert!(ancestors.contains_key(key));
        }
        for slot in min - 1..max + 2 {
            assert_eq!(ancestors.contains_key(&slot), hashset.contains(&slot));
        }
    }

    #[test]
    fn test_ancestors_smaller() {
        for width in 0..34 {
            let mut hash = HashSet::new();

            let min = 1_010_000;
            let dead = 19;

            let mut slot = min;
            let mut slots = Vec::new();
            while hash.len() < width {
                slot += 1;
                if slot % dead == 0 {
                    continue;
                }
                hash.insert(slot);
                slots.push(slot);
            }
            let ancestors = Ancestors::from(slots);

            let max = slot + 1;
            let passes = 1;
            let mut time = Measure::start("");
            let mut count = 0;
            for _pass in 0..passes {
                for slot in (min - 10)..max + 100 {
                    if hash.contains(&slot) {
                        count += 1;
                    }
                }
            }
            time.stop();

            let mut time2 = Measure::start("");
            let mut count2 = 0;
            for _pass in 0..passes {
                for slot in (min - 10)..max + 100 {
                    if ancestors.contains_key(&slot) {
                        count2 += 1;
                    }
                }
            }
            time2.stop();
            info!(
                "{}, {}, {}",
                time.as_ms(),
                time2.as_ms(),
                time.as_ns() / time2.as_ns()
            );
            assert_eq!(count, count2);
        }
    }

    #[test]
    fn test_ancestors_iter_matches_keys() {
        let ancestors = Ancestors::from(vec![3, 42, 128_007, 128_017, 128_107]);

        let mut keys = ancestors.keys();
        let mut iter_keys = ancestors.iter().collect::<Vec<_>>();
        keys.sort_unstable();
        iter_keys.sort_unstable();

        assert_eq!(iter_keys, keys);
    }
}
