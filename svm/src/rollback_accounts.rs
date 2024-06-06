use {
    crate::nonce_info::{NonceInfo, NoncePartial},
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount, WritableAccount},
        pubkey::Pubkey,
    },
};

/// Captured account state used to rollback account state for nonce and fee
/// payer accounts after a failed executed transaction.
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum RollbackAccounts {
    FeePayerOnly {
        fee_payer_account: AccountSharedData,
    },
    SameNonceAndFeePayer {
        nonce: NoncePartial,
    },
    SeparateNonceAndFeePayer {
        nonce: NoncePartial,
        fee_payer_account: AccountSharedData,
    },
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for RollbackAccounts {
    fn default() -> Self {
        Self::FeePayerOnly {
            fee_payer_account: AccountSharedData::default(),
        }
    }
}

impl RollbackAccounts {
    pub fn new(
        nonce: Option<NoncePartial>,
        fee_payer_address: Pubkey,
        mut fee_payer_account: AccountSharedData,
        fee_payer_rent_debit: u64,
        fee_payer_loaded_rent_epoch: u64,
    ) -> Self {
        // When the fee payer account is rolled back due to transaction failure,
        // rent should not be charged so credit the previously debited rent
        // amount.
        fee_payer_account.set_lamports(
            fee_payer_account
                .lamports()
                .saturating_add(fee_payer_rent_debit),
        );

        if let Some(nonce) = nonce {
            if &fee_payer_address == nonce.address() {
                RollbackAccounts::SameNonceAndFeePayer {
                    nonce: NoncePartial::new(fee_payer_address, fee_payer_account),
                }
            } else {
                RollbackAccounts::SeparateNonceAndFeePayer {
                    nonce,
                    fee_payer_account,
                }
            }
        } else {
            // When rolling back failed transactions which don't use nonces, the
            // runtime should not update the fee payer's rent epoch so reset the
            // rollback fee payer acocunt's rent epoch to its originally loaded
            // rent epoch value. In the future, a feature gate could be used to
            // alter this behavior such that rent epoch updates are handled the
            // same for both nonce and non-nonce failed transactions.
            fee_payer_account.set_rent_epoch(fee_payer_loaded_rent_epoch);
            RollbackAccounts::FeePayerOnly { fee_payer_account }
        }
    }

    pub fn nonce(&self) -> Option<&NoncePartial> {
        match self {
            Self::FeePayerOnly { .. } => None,
            Self::SameNonceAndFeePayer { nonce } | Self::SeparateNonceAndFeePayer { nonce, .. } => {
                Some(nonce)
            }
        }
    }

    pub fn fee_payer_account(&self) -> &AccountSharedData {
        match self {
            Self::FeePayerOnly { fee_payer_account }
            | Self::SeparateNonceAndFeePayer {
                fee_payer_account, ..
            } => fee_payer_account,
            Self::SameNonceAndFeePayer { nonce } => nonce.account(),
        }
    }
}
