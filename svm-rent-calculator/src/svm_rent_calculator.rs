//! Helpers for rent calculation within the Solana VM.

use {
    crate::rent_state::RentState,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_transaction_context::{IndexOfAccount, TransactionContext},
    solana_transaction_error::{TransactionError, TransactionResult},
};

/// Check rent state transition for an account in a transaction.
///
/// This method has a default implementation that calls into
/// `check_rent_state_with_account`.
pub fn check_rent_state(
    pre_rent_state: Option<&RentState>,
    post_rent_state: Option<&RentState>,
    transaction_context: &TransactionContext,
    index: IndexOfAccount,
) -> TransactionResult<()> {
    if let Some((pre_rent_state, post_rent_state)) = pre_rent_state.zip(post_rent_state) {
        let expect_msg = "account must exist at TransactionContext index if rent-states are Some";
        check_rent_state_with_account(
            pre_rent_state,
            post_rent_state,
            transaction_context
                .get_key_of_account_at_index(index)
                .expect(expect_msg),
            &transaction_context
                .accounts()
                .try_borrow(index)
                .expect(expect_msg),
            index,
        )?;
    }
    Ok(())
}

/// Check rent state transition for an account directly.
///
/// This method has a default implementation that checks whether the
/// transition is allowed and returns an error if it is not. It also
/// verifies that the account is not the incinerator.
pub fn check_rent_state_with_account(
    pre_rent_state: &RentState,
    post_rent_state: &RentState,
    address: &Pubkey,
    _account_state: &AccountSharedData,
    account_index: IndexOfAccount,
) -> TransactionResult<()> {
    if !solana_sdk_ids::incinerator::check_id(address)
        && !transition_allowed(pre_rent_state, post_rent_state)
    {
        let account_index = account_index as u8;
        Err(TransactionError::InsufficientFundsForRent { account_index })
    } else {
        Ok(())
    }
}

/// Determine the rent state of an account.
///
/// This method has a default implementation that treats accounts with zero
/// lamports as uninitialized and uses the implemented `get_rent` to
/// determine whether an account is rent-exempt.
pub fn get_account_rent_state(rent: &Rent, account: &AccountSharedData) -> RentState {
    if account.lamports() == 0 {
        RentState::Uninitialized
    } else if rent.is_exempt(account.lamports(), account.data().len()) {
        RentState::RentExempt
    } else {
        RentState::RentPaying {
            data_size: account.data().len(),
            lamports: account.lamports(),
        }
    }
}

/// Check whether a transition from the pre_rent_state to the
/// post_rent_state is valid.
///
/// This method has a default implementation that allows transitions from
/// any state to `RentState::Uninitialized` or `RentState::RentExempt`.
/// Pre-state `RentState::RentPaying` can only transition to
/// `RentState::RentPaying` if the data size remains the same and the
/// account is not credited.
pub fn transition_allowed(pre_rent_state: &RentState, post_rent_state: &RentState) -> bool {
    match post_rent_state {
        RentState::Uninitialized | RentState::RentExempt => true,
        RentState::RentPaying {
            data_size: post_data_size,
            lamports: post_lamports,
        } => {
            match pre_rent_state {
                RentState::Uninitialized | RentState::RentExempt => false,
                RentState::RentPaying {
                    data_size: pre_data_size,
                    lamports: pre_lamports,
                } => {
                    // Cannot remain RentPaying if resized or credited.
                    post_data_size == pre_data_size && post_lamports <= pre_lamports
                }
            }
        }
    }
}
