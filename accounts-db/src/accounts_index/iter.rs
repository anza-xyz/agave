use {
    super::{in_mem_accounts_index::InMemAccountsIndex, AccountsIndex, DiskIndexValue, IndexValue},
    solana_pubkey::Pubkey,
    std::{
        ops::{Bound, RangeBounds},
        sync::Arc,
    },
};

pub const ITER_BATCH_SIZE: usize = 1000;

pub struct AccountsIndexPubkeyIterator<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    account_maps: &'a [Arc<InMemAccountsIndex<T, U>>],
    start_bound: Bound<&'a Pubkey>,
    end_bound: Bound<&'a Pubkey>,
    start_bin: usize,
    end_bin_inclusive: usize,
    items: Vec<Pubkey>,
    iter_order: AccountsIndexPubkeyIterOrder,
}

impl<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>
    AccountsIndexPubkeyIterator<'a, T, U>
{
    pub fn new<R>(
        index: &'a AccountsIndex<T, U>,
        range: Option<&'a R>,
        iter_order: AccountsIndexPubkeyIterOrder,
    ) -> Self
    where
        R: RangeBounds<Pubkey>,
    {
        match range {
            Some(range) => {
                let (start_bin, end_bin_inclusive) = index.bin_start_end_inclusive(range);
                Self {
                    account_maps: &index.account_maps,
                    start_bound: range.start_bound(),
                    end_bound: range.end_bound(),
                    start_bin,
                    end_bin_inclusive,
                    items: Vec::new(),
                    iter_order,
                }
            }
            None => Self {
                account_maps: &index.account_maps,
                start_bound: Bound::Unbounded,
                end_bound: Bound::Unbounded,
                start_bin: 0,
                end_bin_inclusive: index.account_maps.len().saturating_sub(1),
                items: Vec::new(),
                iter_order,
            },
        }
    }
}

/// Implement the Iterator trait for AccountsIndexIterator
impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> Iterator
    for AccountsIndexPubkeyIterator<'_, T, U>
{
    type Item = Vec<Pubkey>;
    fn next(&mut self) -> Option<Self::Item> {
        let range = (self.start_bound, self.end_bound);
        while self.items.len() < ITER_BATCH_SIZE {
            if self.start_bin > self.end_bin_inclusive {
                break;
            }

            let bin = self.start_bin;
            let map = &self.account_maps[bin];
            let mut items = map.keys();
            if !(self.start_bound == Bound::Unbounded && self.end_bound == Bound::Unbounded) {
                items.retain(|k| range.contains(k));
            }
            if self.iter_order == AccountsIndexPubkeyIterOrder::Sorted {
                items.sort_unstable();
            }
            self.items.append(&mut items);
            self.start_bin += 1;
        }

        (!self.items.is_empty()).then(|| std::mem::take(&mut self.items))
    }
}

/// Specify how the accounts index pubkey iterator should return pubkeys
///
/// Users should prefer `Unsorted`, unless required otherwise,
/// as sorting incurs additional runtime cost.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AccountsIndexPubkeyIterOrder {
    /// Returns pubkeys *not* sorted
    Unsorted,
    /// Returns pubkeys *sorted*
    Sorted,
}

#[cfg(test)]
mod tests {
    use {
        super::{
            super::{secondary::AccountSecondaryIndexes, UpsertReclaim},
            *,
        },
        crate::accounts_index::ReclaimsSlotList,
        solana_account::AccountSharedData,
        std::ops::Range,
    };

    #[test]
    fn test_account_index_iter_batched() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        // Setup an account index for test.
        // Two bins. First bin has 2000 accounts, second bin has 0 accounts.
        let num_pubkeys = 2 * ITER_BATCH_SIZE;
        let pubkeys = std::iter::repeat_with(Pubkey::new_unique)
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

        for iter_order in [
            AccountsIndexPubkeyIterOrder::Sorted,
            AccountsIndexPubkeyIterOrder::Unsorted,
        ] {
            // Create a sorted iterator for the whole pubkey range.
            let mut iter = index.iter(None::<&Range<Pubkey>>, iter_order);
            // First iter.next() should return the first batch of 2000 pubkeys in the first bin.
            let x = iter.next().unwrap();
            assert_eq!(x.len(), 2 * ITER_BATCH_SIZE);
            assert_eq!(
                x.is_sorted(),
                iter_order == AccountsIndexPubkeyIterOrder::Sorted
            );
            assert_eq!(iter.items.len(), 0); // should be empty.

            // Then iter.next() should return None.
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_accounts_iter_finished() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        index.add_root(0);
        let mut iter = index.iter(None::<&Range<Pubkey>>, AccountsIndexPubkeyIterOrder::Sorted);
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
