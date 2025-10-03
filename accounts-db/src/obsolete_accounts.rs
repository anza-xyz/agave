use {crate::account_info::Offset, solana_clock::Slot};

#[derive(Debug, Clone, PartialEq)]
struct ObsoleteAccountItem {
    /// Offset of the account in the account storage entry
    offset: Offset,
    /// Length of the account data
    data_len: usize,
    /// Slot when the account was marked obsolete
    slot: Slot,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObsoleteAccounts {
    accounts: Vec<ObsoleteAccountItem>,
}

impl ObsoleteAccounts {
    /// Marks the accounts at the given offsets as obsolete
    pub fn mark_accounts_obsolete(
        &mut self,
        newly_obsolete_accounts: impl ExactSizeIterator<Item = (Offset, usize)>,
        slot: Slot,
    ) {
        self.accounts.reserve(newly_obsolete_accounts.len());

        for (offset, data_len) in newly_obsolete_accounts {
            self.accounts.push(ObsoleteAccountItem {
                offset,
                data_len,
                slot,
            });
        }
    }

    /// Returns the accounts that were marked obsolete as of the passed in slot
    /// or earlier. If slot is None, then slot will be assumed to be the max root
    /// and all obsolete accounts will be returned.
    pub fn filter_obsolete_accounts(
        &self,
        slot: Option<Slot>,
    ) -> impl Iterator<Item = (Offset, usize)> + '_ {
        self.accounts
            .iter()
            .filter(move |obsolete_account| slot.is_none_or(|s| obsolete_account.slot <= s))
            .map(|obsolete_account| (obsolete_account.offset, obsolete_account.data_len))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_accounts_obsolete() {
        let mut obsolete_accounts = ObsoleteAccounts::default();
        let new_accounts = vec![(10, 100), (20, 200), (30, 300)];
        let slot: Slot = 42;

        obsolete_accounts.mark_accounts_obsolete(new_accounts.into_iter(), slot);

        let expected_accounts = vec![(10, 100), (20, 200), (30, 300)];

        let actual_accounts: Vec<_> = obsolete_accounts
            .accounts
            .iter()
            .map(|item| (item.offset, item.data_len))
            .collect();

        assert_eq!(actual_accounts, expected_accounts);
    }

    #[test]
    fn test_filter_obsolete_accounts() {
        let mut obsolete_accounts = ObsoleteAccounts::default();
        let new_accounts = vec![(10, 100, 40), (20, 200, 42), (30, 300, 44)]
            .into_iter()
            .map(|(offset, data_len, slot)| ObsoleteAccountItem {
                offset,
                data_len,
                slot,
            })
            .collect::<Vec<_>>();

        // Mark accounts obsolete with different slots
        new_accounts.into_iter().for_each(|item| {
            obsolete_accounts
                .mark_accounts_obsolete([(item.offset, item.data_len)].into_iter(), item.slot)
        });

        // Filter accounts obsolete as of slot 42
        let filtered_accounts: Vec<_> = obsolete_accounts
            .filter_obsolete_accounts(Some(42))
            .collect();

        assert_eq!(filtered_accounts, vec![(10, 100), (20, 200)]);

        // Filter accounts obsolete passing in no slot (i.e., all obsolete accounts)
        let filtered_accounts: Vec<_> = obsolete_accounts.filter_obsolete_accounts(None).collect();

        assert_eq!(filtered_accounts, vec![(10, 100), (20, 200), (30, 300)]);
    }
}
