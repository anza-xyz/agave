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
    iter_order: AccountsIndexPubkeyIterOrder,
}

impl<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>
    AccountsIndexPubkeyIterator<'a, T, U>
{
    pub fn new(index: &'a AccountsIndex<T, U>, iter_order: AccountsIndexPubkeyIterOrder) -> Self {
        Self {
            account_maps: &index.account_maps,
            current_bin: 0,
            items: Vec::new(),
            iter_order,
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
            if self.iter_order == AccountsIndexPubkeyIterOrder::Sorted {
                items.sort_unstable();
            }
            self.items.append(&mut items);
            self.current_bin += 1;
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
            super::{UpsertReclaim, secondary::AccountSecondaryIndexes},
            *,
        },
        crate::accounts_index::ReclaimsSlotList,
        solana_account::AccountSharedData,
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
            let mut iter = index.iter(iter_order);
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
        let mut iter = index.iter(AccountsIndexPubkeyIterOrder::Sorted);
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
