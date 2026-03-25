use {
    super::{AccountsIndex, DiskIndexValue, IndexValue, in_mem_accounts_index::InMemAccountsIndex},
    solana_pubkey::Pubkey,
    std::sync::Arc,
};

pub const ITER_BATCH_SIZE: usize = 1000;

pub struct AccountsIndexPubkeyIterator<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    account_maps: &'a [Arc<InMemAccountsIndex<T, U>>],
    current_bin: usize,
    items: Vec<Pubkey>,
}

impl<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>
    AccountsIndexPubkeyIterator<'a, T, U>
{
    pub fn new(index: &'a AccountsIndex<T, U>) -> Self {
        Self {
            account_maps: &index.account_maps,
            current_bin: 0,
            items: Vec::new(),
        }
    }
}

/// Implement the Iterator trait for AccountsIndexIterator
impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> Iterator
    for AccountsIndexPubkeyIterator<'_, T, U>
{
    type Item = Vec<Pubkey>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.items.len() < ITER_BATCH_SIZE {
            if self.current_bin >= self.account_maps.len() {
                break;
            }

            let map = &self.account_maps[self.current_bin];
            let mut items = map.keys();
            self.items.append(&mut items);
            self.current_bin += 1;
        }

        (!self.items.is_empty()).then(|| std::mem::take(&mut self.items))
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

    /// Ensure that when there are fewer than ITER_BATCH_SIZE items in a bin that `next()`
    /// will correctly get items from the next bin, in a loop, until the iterator has
    /// collected at least ITER_BATCH_SIZE items, or it has visited all the bins.
    #[test]
    fn test_accounts_index_iter_batched_small() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        // this test requires the index to have more than one bin
        assert!(index.bins() > 1);
        // ensure each bin ends up with fewer than ITER_BATCH_SIZE items
        let num_pubkeys = ITER_BATCH_SIZE;
        let pubkeys = iter::repeat_with(solana_pubkey::new_rand)
            .take(num_pubkeys)
            .collect::<Vec<_>>();

        for key in pubkeys {
            let slot = 0;
            let value = true;
            let mut gc = ReclaimsSlotList::new();
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut gc,
                UpsertReclaim::PopulateReclaims,
            );
        }

        // Create an iterator for the whole pubkey range.
        let mut iter = index.iter();
        // First iter.next() should return all the pubkeys
        let x = iter.next().unwrap();
        assert_eq!(x.len(), num_pubkeys);
        assert_eq!(iter.items.len(), 0); // should be empty.

        // Then iter.next() should return None.
        assert!(iter.next().is_none());
    }

    /// Ensure that when there are at least ITER_BATCH_SIZE items in a bin that `next()`
    /// will return those items immediately and *not* visit the next bin.
    #[test]
    fn test_accounts_index_iter_batched_large() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        // this test requires the index to have two bins
        assert_eq!(index.bins(), 2);
        // ensure each bin ends up with more than ITER_BATCH_SIZE items
        let num_pubkeys = ITER_BATCH_SIZE * (index.bins() + 1);
        let pubkeys = iter::repeat_with(solana_pubkey::new_rand)
            .take(num_pubkeys)
            .collect::<Vec<_>>();

        for key in pubkeys {
            let slot = 0;
            let value = true;
            let mut gc = ReclaimsSlotList::new();
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut gc,
                UpsertReclaim::PopulateReclaims,
            );
        }

        // Create an iterator for the whole pubkey range.
        let mut iter = index.iter();
        // First iter.next() should return the whole first bin.
        let x = iter.next().unwrap();
        let (len0, _) = index.account_maps[0].len_and_cap_for_startup();
        assert_eq!(x.len(), len0);
        assert_eq!(iter.items.len(), 0); // should be empty.

        // Second iter.next() should return all the remaining
        let num_remaining = num_pubkeys - len0;
        let y = iter.next().unwrap();
        let (len1, _) = index.account_maps[1].len_and_cap_for_startup();
        assert_eq!(y.len(), num_remaining);
        assert_eq!(y.len(), len1);
        assert_eq!(iter.items.len(), 0); // should be empty.

        // Third iter.next() should return None.
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_accounts_iter_finished() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        index.add_root(0);
        let mut iter = index.iter();
        assert!(iter.next().is_none());
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
        assert!(iter.next().is_none());
    }
}
