//! Solana SVM Rent Calculator.
//!
//! Rent management for SVM.

use {
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_transaction_context::{IndexOfAccount, transaction::TransactionContext},
    solana_transaction_error::{TransactionError, TransactionResult},
};

/// When rent is collected from an exempt account, rent_epoch is set to this
/// value. The idea is to have a fixed, consistent value for rent_epoch for all accounts that do not collect rent.
/// This enables us to get rid of the field completely.
pub const RENT_EXEMPT_RENT_EPOCH: Epoch = Epoch::MAX;

/// Rent state of a Solana account.
#[derive(Debug, PartialEq, Eq)]
pub enum RentState {
    /// account.lamports == 0
    Uninitialized,
    /// 0 < account.lamports < rent-exempt-minimum
    RentPaying {
        lamports: u64,    // account.lamports()
        data_size: usize, // account.data().len()
    },
    /// account.lamports >= rent-exempt-minimum
    RentExempt,
}

/// Check rent state transition for an account in a transaction.
///
/// This method has a default implementation that calls into
/// `check_rent_state_with_account`.
pub fn check_rent_state(
    pre_rent_state: &RentState,
    post_rent_state: &RentState,
    transaction_context: &TransactionContext,
    index: IndexOfAccount,
) -> TransactionResult<()> {
    let expect_msg = "account must exist at TransactionContext index";
    check_rent_state_with_account(
        pre_rent_state,
        post_rent_state,
        transaction_context
            .get_key_of_account_at_index(index)
            .expect(expect_msg),
        index,
    )?;
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
/// lamports as uninitialized and uses the provided `min_balance` to
/// determine whether an account is rent-exempt.
pub fn get_account_rent_state(
    account_lamports: u64,
    account_size: usize,
    min_balance: u64,
) -> RentState {
    if account_lamports == 0 {
        RentState::Uninitialized
    } else if account_lamports >= min_balance {
        RentState::RentExempt
    } else {
        RentState::RentPaying {
            data_size: account_size,
            lamports: account_lamports,
        }
    }
}

/// Determine the pre-execution rent state of an account.
///
/// Equivalent to get_account_rent_state, with the additional option of
/// disallowing the RentPaying state. If disallow_rent_paying is true, accounts
/// that would previously be designated RentPaying will instead be RentExempt.
pub fn get_pre_exec_account_rent_state(
    account_lamports: u64,
    account_size: usize,
    min_balance: u64,
    disallow_rent_paying: bool,
) -> RentState {
    match get_account_rent_state(account_lamports, account_size, min_balance) {
        RentState::RentPaying { .. } if disallow_rent_paying => RentState::RentExempt,
        rent_state => rent_state,
    }
}

/// Determine the post-execution rent state of an account.
///
/// Equivalent to get_account_rent_state, with the additional option of
/// relaxing the criteria to designate an account as RentExempt. The relaxed
/// designation follows the rules specified by SIMD-392.
pub fn get_post_exec_account_rent_state(
    account_lamports: u64,
    account_size: usize,
    min_balance: u64,
    pre_rent_state: &RentState,
    pre_exec_balance: u64,
    relax_rent_exempt_criteria: bool,
) -> RentState {
    if !relax_rent_exempt_criteria {
        return get_account_rent_state(account_lamports, account_size, min_balance);
    };

    match (account_lamports, &pre_rent_state) {
        (0, _) => RentState::Uninitialized,
        (post_balance, _) if post_balance >= min_balance => RentState::RentExempt,
        (post_balance, RentState::RentExempt) if post_balance >= pre_exec_balance => {
            RentState::RentExempt
        }
        (post_balance, _) => RentState::RentPaying {
            data_size: account_size,
            lamports: post_balance,
        },
    }
}

/// Returns whether a transition from the pre_exec_balance to the
/// post_exec_balance is valid for an account that hasn't changed owner
/// or data size.
pub fn check_static_account_rent_state_transition(
    pre_exec_balance: u64,
    post_exec_balance: u64,
    data_size: usize,
    rent: &Rent,
    account_index: IndexOfAccount,
    relax_post_exec_min_balance_check: bool,
) -> TransactionResult<()> {
    let rent_min_balance = rent.minimum_balance(data_size);
    let pre_state = get_pre_exec_account_rent_state(
        pre_exec_balance,
        data_size,
        rent_min_balance,
        // post SIMD-392 activation, RentPaying accounts are no longer possible
        relax_post_exec_min_balance_check,
    );
    let post_state = get_post_exec_account_rent_state(
        post_exec_balance,
        data_size,
        rent_min_balance,
        &pre_state,
        pre_exec_balance,
        relax_post_exec_min_balance_check,
    );

    if !transition_allowed(&pre_state, &post_state) {
        let account_index = account_index as u8;
        Err(TransactionError::InsufficientFundsForRent { account_index })
    } else {
        Ok(())
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
