use {
    solana_instruction::error::InstructionError,
    solana_nonce::{
        state::{DurableNonce, State},
        versions::Versions,
    },
    solana_program_runtime::invoke_context::InvokeContext,
    solana_pubkey::Pubkey,
    solana_svm_log_collector::ic_msg,
    solana_system_interface::error::SystemError,
    solana_sysvar::rent::Rent,
    solana_transaction_context::{IndexOfAccount, instruction::InstructionContext},
    std::collections::HashSet,
};

/// Addition that returns [`InstructionError::InsufficientFunds`] on overflow.
fn checked_add(a: u64, b: u64) -> Result<u64, InstructionError> {
    a.checked_add(b).ok_or(InstructionError::InsufficientFunds)
}

pub(crate) fn withdraw_nonce_account(
    from_account_index: IndexOfAccount,
    lamports: u64,
    to_account_index: IndexOfAccount,
    rent: &Rent,
    signers: &HashSet<Pubkey>,
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
) -> Result<(), InstructionError> {
    let mut from = instruction_context.try_borrow_instruction_account(from_account_index)?;
    if !from.is_writable() {
        ic_msg!(
            invoke_context,
            "Withdraw nonce account: Account {} must be writeable",
            from.get_key()
        );
        return Err(InstructionError::InvalidArgument);
    }

    let check_signer = |signer: &Pubkey| {
        if !signers.contains(signer) {
            ic_msg!(
                invoke_context,
                "Withdraw nonce account: Account {} must sign",
                signer
            );
            return Err(InstructionError::MissingRequiredSignature);
        }
        Ok(())
    };

    let state: Versions = from.get_state()?;
    match state.state() {
        State::Uninitialized => {
            if lamports > from.get_lamports() {
                ic_msg!(
                    invoke_context,
                    "Withdraw nonce account: insufficient lamports {}, need {}",
                    from.get_lamports(),
                    lamports,
                );
                return Err(InstructionError::InsufficientFunds);
            }
            check_signer(from.get_key())?;
        }
        State::Initialized(data) => {
            if lamports == from.get_lamports() {
                let durable_nonce =
                    DurableNonce::from_blockhash(&invoke_context.environment_config.blockhash);
                if data.durable_nonce == durable_nonce {
                    ic_msg!(
                        invoke_context,
                        "Withdraw nonce account: nonce can only advance once per slot"
                    );
                    return Err(SystemError::NonceBlockhashNotExpired.into());
                }
                check_signer(&data.authority)?;
                from.set_state(&Versions::new(State::Uninitialized))?;
            } else {
                let min_balance = rent.minimum_balance(from.get_data().len());
                let amount = checked_add(lamports, min_balance)?;
                if amount > from.get_lamports() {
                    ic_msg!(
                        invoke_context,
                        "Withdraw nonce account: insufficient lamports {}, need {}",
                        from.get_lamports(),
                        amount,
                    );
                    return Err(InstructionError::InsufficientFunds);
                }
                check_signer(&data.authority)?;
            }
        }
    };

    from.checked_sub_lamports(lamports)?;
    drop(from);
    let mut to = instruction_context.try_borrow_instruction_account(to_account_index)?;
    to.checked_add_lamports(lamports)?;

    Ok(())
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_account::AccountSharedData,
        solana_hash::Hash,
        solana_nonce::{self as nonce, state::State},
        solana_nonce_account::{create_account, verify_nonce_account},
        solana_program_runtime::with_mock_invoke_context,
        solana_sdk_ids::system_program,
        solana_sha256_hasher::hash,
        solana_transaction_context::instruction_accounts::InstructionAccount,
    };

    pub const NONCE_ACCOUNT_INDEX: IndexOfAccount = 0;
    pub const WITHDRAW_TO_ACCOUNT_INDEX: IndexOfAccount = 1;

    macro_rules! push_instruction_context {
        ($invoke_context:expr, $instruction_context:ident, $instruction_accounts:ident) => {
            $invoke_context
                .transaction_context
                .configure_top_level_instruction_for_tests(2, $instruction_accounts, vec![])
                .unwrap();
            $invoke_context.push().unwrap();
            let transaction_context = &$invoke_context.transaction_context;
            let $instruction_context = transaction_context
                .get_current_instruction_context()
                .unwrap();
        };
    }

    macro_rules! prepare_mockup {
        ($invoke_context:ident, $instruction_accounts:ident, $rent:ident, $transaction_context:ident) => {
            let $rent = Rent {
                lamports_per_byte: 42,
                ..Rent::default()
            };
            let from_lamports = $rent.minimum_balance(State::size()) + 42;
            let transaction_accounts = vec![
                (
                    Pubkey::new_unique(),
                    create_account(from_lamports).into_inner(),
                ),
                (Pubkey::new_unique(), create_account(42).into_inner()),
                (system_program::id(), AccountSharedData::default()),
            ];
            let $instruction_accounts = vec![
                InstructionAccount::new(0, true, true),
                InstructionAccount::new(1, false, true),
            ];
            with_mock_invoke_context!($invoke_context, $transaction_context, transaction_accounts);
        };
    }

    macro_rules! set_invoke_context_blockhash {
        ($invoke_context:expr, $seed:expr) => {
            $invoke_context.environment_config.blockhash =
                hash(&bincode::serialize(&$seed).unwrap());
            $invoke_context
                .environment_config
                .blockhash_lamports_per_signature = ($seed as u64).saturating_mul(100);
        };
    }

    #[test]
    fn default_is_uninitialized() {
        assert_eq!(State::default(), State::Uninitialized)
    }

    #[test]
    fn withdraw_inx_unintialized_acc_ok() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        assert_eq!(versions.state(), &State::Uninitialized);
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        set_invoke_context_blockhash!(invoke_context, 0);
        let withdraw_lamports = nonce_account.get_lamports();
        let expect_from_lamports = nonce_account.get_lamports() - withdraw_lamports;
        let expect_to_lamports = to_account.get_lamports() + withdraw_lamports;
        drop(nonce_account);
        drop(to_account);
        withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        )
        .unwrap();
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        assert_eq!(versions.state(), &State::Uninitialized);
        assert_eq!(nonce_account.get_lamports(), expect_from_lamports);
        assert_eq!(to_account.get_lamports(), expect_to_lamports);
    }

    #[test]
    fn withdraw_inx_unintialized_acc_unsigned_fail() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        assert_eq!(versions.state(), &State::Uninitialized);
        let signers = HashSet::new();
        set_invoke_context_blockhash!(invoke_context, 0);
        let withdraw_lamports = nonce_account.get_lamports();
        drop(nonce_account);
        drop(to_account);
        let result = withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        );
        assert_eq!(result, Err(InstructionError::MissingRequiredSignature));
    }

    #[test]
    fn withdraw_inx_unintialized_acc_insuff_funds_fail() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        assert_eq!(versions.state(), &State::Uninitialized);
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        set_invoke_context_blockhash!(invoke_context, 0);
        let withdraw_lamports = nonce_account.get_lamports() + 1;
        drop(nonce_account);
        let result = withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        );
        assert_eq!(result, Err(InstructionError::InsufficientFunds));
    }

    #[test]
    fn withdraw_inx_uninitialized_acc_two_withdraws_ok() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        set_invoke_context_blockhash!(invoke_context, 0);
        let withdraw_lamports = nonce_account.get_lamports() / 2;
        let from_expect_lamports = nonce_account.get_lamports() - withdraw_lamports;
        let to_expect_lamports = to_account.get_lamports() + withdraw_lamports;
        drop(nonce_account);
        drop(to_account);
        withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        )
        .unwrap();
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        assert_eq!(versions.state(), &State::Uninitialized);
        assert_eq!(nonce_account.get_lamports(), from_expect_lamports);
        assert_eq!(to_account.get_lamports(), to_expect_lamports);
        let withdraw_lamports = nonce_account.get_lamports();
        let from_expect_lamports = nonce_account.get_lamports() - withdraw_lamports;
        let to_expect_lamports = to_account.get_lamports() + withdraw_lamports;
        drop(nonce_account);
        drop(to_account);
        withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        )
        .unwrap();
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        assert_eq!(versions.state(), &State::Uninitialized);
        assert_eq!(nonce_account.get_lamports(), from_expect_lamports);
        assert_eq!(to_account.get_lamports(), to_expect_lamports);
    }

    #[test]
    fn withdraw_inx_initialized_acc_two_withdraws_ok() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let mut nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        set_invoke_context_blockhash!(invoke_context, 31);
        let authority = *nonce_account.get_key();
        let data = nonce::state::Data::new(
            authority,
            DurableNonce::from_blockhash(&invoke_context.environment_config.blockhash),
            invoke_context
                .environment_config
                .blockhash_lamports_per_signature,
        );
        nonce_account
            .set_state(&Versions::new(State::Initialized(data.clone())))
            .unwrap();
        let withdraw_lamports = 42;
        let from_expect_lamports = nonce_account.get_lamports() - withdraw_lamports;
        let to_expect_lamports = to_account.get_lamports() + withdraw_lamports;
        drop(nonce_account);
        drop(to_account);
        withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        )
        .unwrap();
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        let data = nonce::state::Data::new(
            data.authority,
            DurableNonce::from_blockhash(&invoke_context.environment_config.blockhash),
            invoke_context
                .environment_config
                .blockhash_lamports_per_signature,
        );
        assert_eq!(versions.state(), &State::Initialized(data));
        assert_eq!(nonce_account.get_lamports(), from_expect_lamports);
        assert_eq!(to_account.get_lamports(), to_expect_lamports);
        set_invoke_context_blockhash!(invoke_context, 0);
        let withdraw_lamports = nonce_account.get_lamports();
        let from_expect_lamports = nonce_account.get_lamports() - withdraw_lamports;
        let to_expect_lamports = to_account.get_lamports() + withdraw_lamports;
        drop(nonce_account);
        drop(to_account);
        withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        )
        .unwrap();
        let nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        let versions = nonce_account.get_state::<Versions>().unwrap();
        assert_eq!(versions.state(), &State::Uninitialized);
        assert_eq!(nonce_account.get_lamports(), from_expect_lamports);
        assert_eq!(to_account.get_lamports(), to_expect_lamports);
    }

    #[test]
    fn withdraw_inx_initialized_acc_nonce_too_early_fail() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let mut nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        let to_account = instruction_context
            .try_borrow_instruction_account(WITHDRAW_TO_ACCOUNT_INDEX)
            .unwrap();
        set_invoke_context_blockhash!(invoke_context, 0);
        let data = nonce::state::Data::new(
            *nonce_account.get_key(),
            DurableNonce::from_blockhash(&invoke_context.environment_config.blockhash),
            invoke_context
                .environment_config
                .blockhash_lamports_per_signature,
        );
        nonce_account
            .set_state(&Versions::new(State::Initialized(data)))
            .unwrap();
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        let withdraw_lamports = nonce_account.get_lamports();
        drop(nonce_account);
        drop(to_account);
        let result = withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        );
        assert_eq!(result, Err(SystemError::NonceBlockhashNotExpired.into()));
    }

    #[test]
    fn withdraw_inx_initialized_acc_insuff_funds_fail() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let mut nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        set_invoke_context_blockhash!(invoke_context, 95);
        let data = nonce::state::Data::new(
            *nonce_account.get_key(),
            DurableNonce::from_blockhash(&invoke_context.environment_config.blockhash),
            invoke_context
                .environment_config
                .blockhash_lamports_per_signature,
        );
        nonce_account
            .set_state(&Versions::new(State::Initialized(data)))
            .unwrap();
        set_invoke_context_blockhash!(invoke_context, 63);
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        let withdraw_lamports = nonce_account.get_lamports() + 1;
        drop(nonce_account);
        let result = withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        );
        assert_eq!(result, Err(InstructionError::InsufficientFunds));
    }

    #[test]
    fn withdraw_inx_initialized_acc_insuff_rent_fail() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let mut nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        set_invoke_context_blockhash!(invoke_context, 95);
        let data = nonce::state::Data::new(
            *nonce_account.get_key(),
            DurableNonce::from_blockhash(&invoke_context.environment_config.blockhash),
            invoke_context
                .environment_config
                .blockhash_lamports_per_signature,
        );
        nonce_account
            .set_state(&Versions::new(State::Initialized(data)))
            .unwrap();
        set_invoke_context_blockhash!(invoke_context, 63);
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        let withdraw_lamports = 42 + 1;
        drop(nonce_account);
        let result = withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        );
        assert_eq!(result, Err(InstructionError::InsufficientFunds));
    }

    #[test]
    fn withdraw_inx_overflow() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );
        push_instruction_context!(invoke_context, instruction_context, instruction_accounts);
        let mut nonce_account = instruction_context
            .try_borrow_instruction_account(NONCE_ACCOUNT_INDEX)
            .unwrap();
        set_invoke_context_blockhash!(invoke_context, 95);
        let data = nonce::state::Data::new(
            *nonce_account.get_key(),
            DurableNonce::from_blockhash(&invoke_context.environment_config.blockhash),
            invoke_context
                .environment_config
                .blockhash_lamports_per_signature,
        );
        nonce_account
            .set_state(&Versions::new(State::Initialized(data)))
            .unwrap();
        set_invoke_context_blockhash!(invoke_context, 63);
        let mut signers = HashSet::new();
        signers.insert(*nonce_account.get_key());
        let withdraw_lamports = u64::MAX - 54;
        drop(nonce_account);
        let result = withdraw_nonce_account(
            NONCE_ACCOUNT_INDEX,
            withdraw_lamports,
            WITHDRAW_TO_ACCOUNT_INDEX,
            &rent,
            &signers,
            &invoke_context,
            &instruction_context,
        );
        assert_eq!(result, Err(InstructionError::InsufficientFunds));
    }

    #[test]
    fn verify_nonce_bad_acc_state_fail() {
        prepare_mockup!(
            invoke_context,
            instruction_accounts,
            rent,
            transaction_context
        );

        {
            push_instruction_context!(invoke_context, _instruction_context, instruction_accounts);
        }

        transaction_context.pop().unwrap();
        let post_accounts = transaction_context.deconstruct_without_keys().unwrap();
        assert_eq!(
            verify_nonce_account(
                post_accounts.get(NONCE_ACCOUNT_INDEX as usize).unwrap(),
                &Hash::default(),
            ),
            None
        );
    }
}
