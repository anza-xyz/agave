use {
    crate::rolling_bit_field::RollingBitField,
    core::fmt::{Debug, Formatter},
    solana_clock::Slot,
};

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, PartialEq)]
pub struct Ancestors {
    ancestors: RollingBitField,
}

impl Debug for Ancestors {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.keys())
    }
}

// some tests produce ancestors ranges that are too large such
// that we prefer to implement them in a sparse HashSet
const ANCESTORS_SIZE: u64 = 8192;

impl Default for Ancestors {
    fn default() -> Self {
        Self {
            ancestors: RollingBitField::new(ANCESTORS_SIZE),
        }
    }
}

impl From<Vec<Slot>> for Ancestors {
    fn from(mut source: Vec<Slot>) -> Ancestors {
        // bitfield performs optimally when we insert the minimum value first so that it knows the correct start/end values
        source.sort_unstable();
        let mut result = Ancestors::default();
        source.into_iter().for_each(|slot| {
            result.ancestors.insert(slot);
        });

        result
    }
}

impl Ancestors {
    pub fn keys(&self) -> Vec<Slot> {
        self.ancestors.get_all()
    }

    pub fn remove(&mut self, slot: &Slot) {
        self.ancestors.remove(slot);
    }

    pub fn contains_key(&self, slot: &Slot) -> bool {
        self.ancestors.contains(slot)
    }

    pub fn len(&self) -> usize {
        self.ancestors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn min_slot(&self) -> Option<Slot> {
        self.ancestors.min()
    }

    pub fn max_slot(&self) -> Slot {
        self.ancestors.max_exclusive().saturating_sub(1)
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl Ancestors {
    pub fn insert(&mut self, slot: Slot) {
        self.ancestors.insert(slot);
    }
}

#[cfg(test)]
mod tests {
    use {super::*, log::*, solana_measure::measure::Measure, std::collections::HashSet};

    #[test]
    fn test_ancestors_permutations() {
        agave_logger::setup();
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
        agave_logger::setup();

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
}
