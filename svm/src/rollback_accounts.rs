use {
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_transaction_context::transaction_accounts::KeyedAccountSharedData,
};

/// Captured fee payer account state used to rollback the fee payer after a
/// failed executed transaction.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct RollbackAccounts {
    fee_payer: KeyedAccountSharedData,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for RollbackAccounts {
    fn default() -> Self {
        Self {
            fee_payer: KeyedAccountSharedData::default(),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl RollbackAccounts {
    pub fn new_for_tests(
        fee_payer_address: Pubkey,
        fee_payer_account: AccountSharedData,
        fee_payer_loaded_rent_epoch: Epoch,
    ) -> Self {
        Self::new(
            fee_payer_address,
            fee_payer_account,
            fee_payer_loaded_rent_epoch,
        )
    }
}

/// Rollback accounts iterator.
/// This struct is created by the `RollbackAccounts::iter`.
pub struct RollbackAccountsIter<'a> {
    fee_payer: Option<&'a KeyedAccountSharedData>,
}

impl<'a> Iterator for RollbackAccountsIter<'a> {
    type Item = &'a KeyedAccountSharedData;

    fn next(&mut self) -> Option<Self::Item> {
        self.fee_payer.take()
    }
}

impl<'a> IntoIterator for &'a RollbackAccounts {
    type Item = &'a KeyedAccountSharedData;
    type IntoIter = RollbackAccountsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl RollbackAccounts {
    pub(crate) fn new(
        fee_payer_address: Pubkey,
        mut fee_payer_account: AccountSharedData,
        fee_payer_loaded_rent_epoch: Epoch,
    ) -> Self {
        // When rolling back failed transactions, the runtime should not update
        // the fee payer's rent epoch so reset the rollback fee payer account's
        // rent epoch to its originally loaded rent epoch value.
        fee_payer_account.set_rent_epoch(fee_payer_loaded_rent_epoch);
        Self {
            fee_payer: (fee_payer_address, fee_payer_account),
        }
    }

    /// Return a reference to the fee payer account.
    pub fn fee_payer(&self) -> &KeyedAccountSharedData {
        &self.fee_payer
    }

    /// Number of accounts tracked for rollback
    pub fn count(&self) -> usize {
        1
    }

    /// Iterator over accounts tracked for rollback.
    pub fn iter(&self) -> RollbackAccountsIter<'_> {
        RollbackAccountsIter {
            fee_payer: Some(&self.fee_payer),
        }
    }

    // Size of accounts tracked for rollback, used internally when calculating the actual
    // loaded transaction data size for the cost model. This function will be removed by
    // the fee-payer data size amendment to SIMD-186.
    pub(crate) fn data_size(&self) -> usize {
        let mut total_size: usize = 0;
        for (_, account) in self.iter() {
            total_size = total_size.saturating_add(account.data().len());
        }
        total_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_fee_payer_only() {
        let fee_payer_address = Pubkey::new_unique();
        let fee_payer_account = AccountSharedData::new(100, 0, &Pubkey::default());
        let fee_payer_rent_epoch = fee_payer_account.rent_epoch();

        let rent_epoch_updated_fee_payer_account = {
            let mut account = fee_payer_account.clone();
            account.set_lamports(fee_payer_account.lamports());
            account.set_rent_epoch(fee_payer_rent_epoch + 1);
            account
        };

        let rollback_accounts = RollbackAccounts::new(
            fee_payer_address,
            rent_epoch_updated_fee_payer_account,
            fee_payer_rent_epoch,
        );

        let expected_fee_payer = (fee_payer_address, fee_payer_account);
        assert_eq!(&expected_fee_payer, rollback_accounts.fee_payer());
        assert_eq!(rollback_accounts.count(), 1);
    }
}
