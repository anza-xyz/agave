#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{IndexOfAccount, MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION, MAX_ACCOUNT_DATA_LEN},
    solana_account::AccountSharedData,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    std::cell::{Cell, Ref, RefCell, RefMut},
};

/// An account key and the matching account
pub type TransactionAccount = (Pubkey, AccountSharedData);
pub(crate) type OwnedTransactionAccounts = (
    Vec<RefCell<AccountSharedData>>,
    Box<[Cell<bool>]>,
    Cell<i64>,
);

#[derive(Debug)]
pub struct TransactionAccounts {
    accounts: Vec<RefCell<AccountSharedData>>,
    touched_flags: Box<[Cell<bool>]>,
    resize_delta: Cell<i64>,
    lamports_delta: Cell<i128>,
}

impl TransactionAccounts {
    #[cfg(not(target_os = "solana"))]
    pub(crate) fn new(accounts: Vec<RefCell<AccountSharedData>>) -> TransactionAccounts {
        let touched_flags = vec![Cell::new(false); accounts.len()].into_boxed_slice();
        TransactionAccounts {
            accounts,
            touched_flags,
            resize_delta: Cell::new(0),
            lamports_delta: Cell::new(0),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.accounts.len()
    }

    #[cfg(not(target_os = "solana"))]
    pub fn touch(&self, index: IndexOfAccount) -> Result<(), InstructionError> {
        self.touched_flags
            .get(index as usize)
            .ok_or(InstructionError::NotEnoughAccountKeys)?
            .set(true);
        Ok(())
    }

    pub(crate) fn update_accounts_resize_delta(
        &self,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), InstructionError> {
        let accounts_resize_delta = self.resize_delta.get();
        self.resize_delta.set(
            accounts_resize_delta.saturating_add((new_len as i64).saturating_sub(old_len as i64)),
        );
        Ok(())
    }

    pub(crate) fn can_data_be_resized(
        &self,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), InstructionError> {
        // The new length can not exceed the maximum permitted length
        if new_len > MAX_ACCOUNT_DATA_LEN as usize {
            return Err(InstructionError::InvalidRealloc);
        }
        // The resize can not exceed the per-transaction maximum
        let length_delta = (new_len as i64).saturating_sub(old_len as i64);
        if self.resize_delta.get().saturating_add(length_delta)
            > MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION
        {
            return Err(InstructionError::MaxAccountsDataAllocationsExceeded);
        }
        Ok(())
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn try_borrow_mut(
        &self,
        index: IndexOfAccount,
    ) -> Result<RefMut<'_, AccountSharedData>, InstructionError> {
        self.accounts
            .get(index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .try_borrow_mut()
            .map_err(|_| InstructionError::AccountBorrowFailed)
    }

    pub fn try_borrow(
        &self,
        index: IndexOfAccount,
    ) -> Result<Ref<'_, AccountSharedData>, InstructionError> {
        self.accounts
            .get(index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .try_borrow()
            .map_err(|_| InstructionError::AccountBorrowFailed)
    }

    pub(crate) fn add_lamports_delta(&self, balance: i128) -> Result<(), InstructionError> {
        let delta = self.lamports_delta.get();
        self.lamports_delta.set(
            delta
                .checked_add(balance)
                .ok_or(InstructionError::ArithmeticOverflow)?,
        );
        Ok(())
    }

    pub(crate) fn get_lamports_delta(&self) -> i128 {
        self.lamports_delta.get()
    }

    pub(crate) fn take(self) -> OwnedTransactionAccounts {
        (self.accounts, self.touched_flags, self.resize_delta)
    }

    pub fn resize_delta(&self) -> i64 {
        self.resize_delta.get()
    }
}
