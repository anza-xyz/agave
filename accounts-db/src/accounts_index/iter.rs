use {
    super::{AccountsIndex, DiskIndexValue, IndexValue, in_mem_accounts_index::InMemAccountsIndex},
    solana_pubkey::Pubkey,
    std::sync::Arc,
};

/// Iterator over AccountsIndex
///
/// One bin is inspected per call to `next()`, and bins may be empty.
/// Thus, clients must be able to handle `next()` returning `Some(vec![])`.
pub struct AccountsIndexPubkeyIterator<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    account_maps: &'a [Arc<InMemAccountsIndex<T, U>>],
    current_bin: usize,
}

impl<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>
    AccountsIndexPubkeyIterator<'a, T, U>
{
    pub fn new(index: &'a AccountsIndex<T, U>) -> Self {
        Self {
            account_maps: &index.account_maps,
            current_bin: 0,
        }
    }
}

/// Implement the Iterator trait for AccountsIndexIterator
impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> Iterator
    for AccountsIndexPubkeyIterator<'_, T, U>
{
    type Item = Vec<Pubkey>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.current_bin < self.account_maps.len() {
            let items = self.account_maps[self.current_bin].keys();
            self.current_bin += 1;
            Some(items)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            super::{UpsertReclaim, secondary::AccountSecondaryIndexes},
            *,
        },
        crate::accounts_index::ReclaimsSlotList,
        solana_account::AccountSharedData,
        std::iter,
    };

    /// Ensure iterator visits all bins.
    #[test]
    fn test_visits_all_bins() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();

        let num_bins_visited = index.iter().count();
        assert_eq!(num_bins_visited, index.bins());
    }

    /// Ensure exhausted iterator continues to return None if new entries are added to the index.
    #[test]
    fn test_exhausted() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let mut iter = index.iter();
        for _items in iter.by_ref() {
            // exhaust the iterator
        }
        assert!(iter.next().is_none());

        // add a new entry to the index
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &solana_pubkey::new_rand(),
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );

        // ensure the iterator remains exhausted
        assert!(iter.next().is_none());
    }

    /// Ensure iterators return all the pubkeys, even if some bins are empty.
    #[test]
    fn test_some_empty_bins() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let num_bins = index.bins();
        // need more than one bin to ensure some are empty and some are not
        assert!(num_bins > 1);
        // need at least one empty bin
        let num_pubkeys = num_bins - 1;
        let mut pubkeys: Vec<_> = iter::repeat_with(solana_pubkey::new_rand)
            .take(num_pubkeys)
            .collect();
        // need the pubkeys sorted for assertions later
        pubkeys.sort_unstable();

        for pubkey in &pubkeys {
            let slot = 0;
            let value = true;
            let mut gc = ReclaimsSlotList::new();
            index.upsert(
                slot,
                slot,
                pubkey,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut gc,
                UpsertReclaim::PopulateReclaims,
            );
        }

        let mut num_empty_bins = 0;
        let mut actual_pubkeys = Vec::new();
        for mut items in index.iter() {
            if items.is_empty() {
                num_empty_bins += 1;
            }
            actual_pubkeys.append(&mut items);
        }
        actual_pubkeys.sort_unstable();

        assert_ne!(num_empty_bins, 0);
        assert_eq!(pubkeys, actual_pubkeys);
    }

    /// Ensure iterators return all the pubkeys, when none of the bins are empty.
    #[test]
    fn test_no_empty_bins() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let num_bins = index.bins();
        // need enough pubkeys so no bins are empty
        let num_pubkeys = num_bins * 123;
        let mut pubkeys: Vec<_> = iter::repeat_with(solana_pubkey::new_rand)
            .take(num_pubkeys)
            .collect();
        // need the pubkeys sorted for assertions later
        pubkeys.sort_unstable();

        for pubkey in &pubkeys {
            let slot = 0;
            let value = true;
            let mut gc = ReclaimsSlotList::new();
            index.upsert(
                slot,
                slot,
                pubkey,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut gc,
                UpsertReclaim::PopulateReclaims,
            );
        }

        let mut num_empty_bins = 0;
        let mut actual_pubkeys = Vec::new();
        for mut items in index.iter() {
            if items.is_empty() {
                num_empty_bins += 1;
            }
            actual_pubkeys.append(&mut items);
        }
        actual_pubkeys.sort_unstable();

        assert_eq!(num_empty_bins, 0);
        assert_eq!(pubkeys, actual_pubkeys);
    }
}
