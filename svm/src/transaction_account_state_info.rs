use {
    crate::rent_calculator::{
        RentState, check_rent_state, get_post_exec_account_rent_state,
        get_pre_exec_account_rent_state,
    },
    solana_account::ReadableAccount,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_context::{IndexOfAccount, transaction::TransactionContext},
    solana_transaction_error::TransactionResult as Result,
};

#[derive(PartialEq, Debug)]
pub(crate) struct TransactionAccountStateInfo {
    info: Option<WritableTransactionAccountStateInfo>, // None: readonly account
}

impl TransactionAccountStateInfo {
    pub(crate) fn new_pre_exec(
        transaction_context: &TransactionContext,
        message: &impl SVMMessage,
        rent: &Rent,
        relax_post_exec_min_balance_check: bool,
    ) -> Vec<Self> {
        iter_writable_accounts(transaction_context, message)
            .map(|acct_ref| {
                let info = acct_ref.map(|account| {
                    let balance = account.lamports();
                    let data_size = account.data().len();
                    let owner = *account.owner();
                    let rent_state = get_pre_exec_account_rent_state(
                        balance,
                        data_size,
                        rent.minimum_balance(data_size),
                        relax_post_exec_min_balance_check, // SIMD-0392 enabled. Assume `RentPaying` is impossible
                    );

                    WritableTransactionAccountStateInfo {
                        rent_state,
                        balance,
                        data_size,
                        owner,
                    }
                });
                Self { info }
            })
            .collect()
    }

    pub(crate) fn new_post_exec(
        transaction_context: &TransactionContext,
        message: &impl SVMMessage,
        pre_exec_state_infos: &[TransactionAccountStateInfo],
        rent: &Rent,
        relax_post_exec_min_balance_check: bool,
    ) -> Vec<Self> {
        debug_assert_eq!(pre_exec_state_infos.len(), message.account_keys().len());

        iter_writable_accounts(transaction_context, message)
            .zip(pre_exec_state_infos)
            .map(|(acct_ref, pre_exec_state_info)| {
                let info = acct_ref.and_then(|account| {
                    // the same account MUST be present and marked writable in both pre and post exec
                    debug_assert!(
                        pre_exec_state_info.info.is_some(),
                        "message and pre-exec state out of sync, fatal"
                    );

                    let pre_exec_state_info = pre_exec_state_info.info.as_ref()?;
                    let balance = account.lamports();
                    let data_size = account.data().len();
                    let min_balance = rent.minimum_balance(data_size);
                    let owner = *account.owner();
                    let pre_state = &pre_exec_state_info.rent_state;
                    let pre_balance = pre_exec_state_info.balance;

                    // Criteria for rent-exempt relaxation as specified by SIMD-392
                    let relax_rent_exempt_criteria = relax_post_exec_min_balance_check
                        && pre_exec_state_info.data_size >= data_size
                        && *pre_state == RentState::RentExempt
                        && pre_exec_state_info.owner == owner;

                    // Post-exec owner is currently not consumed by any caller.
                    Some(WritableTransactionAccountStateInfo {
                        rent_state: get_post_exec_account_rent_state(
                            balance,
                            data_size,
                            min_balance,
                            pre_state,
                            pre_balance,
                            relax_rent_exempt_criteria,
                        ),
                        balance,
                        data_size,
                        owner,
                    })
                });
                Self { info }
            })
            .collect()
    }
}

pub(crate) fn verify_changes(
    pre_state_infos: &[TransactionAccountStateInfo],
    post_state_infos: &[TransactionAccountStateInfo],
    transaction_context: &TransactionContext,
) -> Result<()> {
    for (i, (pre_state_info, post_state_info)) in
        pre_state_infos.iter().zip(post_state_infos).enumerate()
    {
        if let (Some(pre_state_info), Some(post_state_info)) =
            (pre_state_info.info.as_ref(), post_state_info.info.as_ref())
        {
            check_rent_state(
                &pre_state_info.rent_state,
                &post_state_info.rent_state,
                transaction_context,
                i as IndexOfAccount,
            )?;
        }
    }
    Ok(())
}

// Returns the cumulative size of all post-exec uninitialized accounts
pub(crate) fn get_uninitialized_accounts_size(post: &[TransactionAccountStateInfo]) -> u64 {
    post.iter()
        .filter_map(|state| state.info.as_ref())
        .filter_map(|post| {
            matches!(&post.rent_state, RentState::Uninitialized).then_some(post.data_size as u64)
        })
        .sum()
}

#[derive(PartialEq, Debug)]
pub(crate) struct WritableTransactionAccountStateInfo {
    rent_state: RentState,
    balance: u64,
    data_size: usize,
    owner: Pubkey,
}

fn iter_writable_accounts<'a>(
    transaction_context: &'a TransactionContext,
    message: &impl SVMMessage,
) -> impl Iterator<Item = Option<solana_transaction_context::transaction_accounts::AccountRef<'a>>>
{
    (0..message.account_keys().len()).map(|i| {
        if message.is_writable(i) {
            let account = transaction_context
                .accounts()
                .try_borrow(i as IndexOfAccount);
            debug_assert!(
                account.is_ok(),
                "message and transaction context out of sync, fatal"
            );
            account.ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_account::{AccountSharedData, WritableAccount},
        solana_hash::Hash,
        solana_message::{
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
            compiled_instruction::CompiledInstruction,
        },
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_transaction_context::transaction::TransactionContext,
        solana_transaction_error::TransactionError,
        std::{collections::HashSet, iter::repeat_with},
    };

    // Helpers to reduce duplication in tests
    fn sanitized_msg_for(
        program_key: Pubkey,
        writable_key: Pubkey,
        other_writable_key: Pubkey,
    ) -> SanitizedMessage {
        let message = Message {
            account_keys: vec![writable_key, program_key, other_writable_key],
            header: MessageHeader::default(),
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![0],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![2],
                    data: vec![],
                },
            ],
            recent_blockhash: Hash::default(),
        };
        SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()))
    }

    #[test]
    fn test_pre_exec_basics() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();

        let sanitized_message = sanitized_msg_for(program, account, other_account);

        let transaction_accounts = vec![
            (account, AccountSharedData::default()),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let default_owner = Pubkey::default();

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20, 2);
        let result =
            TransactionAccountStateInfo::new_pre_exec(&context, &sanitized_message, &rent, true);
        assert_eq!(
            result,
            vec![
                TransactionAccountStateInfo {
                    info: Some(WritableTransactionAccountStateInfo {
                        rent_state: RentState::Uninitialized,
                        balance: 0,
                        data_size: 0,
                        owner: default_owner,
                    }),
                },
                TransactionAccountStateInfo { info: None },
                TransactionAccountStateInfo {
                    info: Some(WritableTransactionAccountStateInfo {
                        rent_state: RentState::Uninitialized,
                        balance: 0,
                        data_size: 0,
                        owner: default_owner,
                    }),
                },
            ]
        );

        // no post-exec in this test
    }

    #[test]
    fn test_pre_exec_legacy_rent_paying() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();

        let data_len = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full - 1;
        // account must be both initialized and have a sub-exempt balance
        assert!(pre_balance > 0);

        let sanitized_message = sanitized_msg_for(program, account, other_account);
        let tx_accounts = vec![
            (
                account,
                AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context = TransactionContext::new(tx_accounts, rent.clone(), 20, 20, 2);
        let pre =
            TransactionAccountStateInfo::new_pre_exec(&context, &sanitized_message, &rent, false);

        assert_eq!(
            pre[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentPaying {
                data_size: data_len,
                lamports: pre_balance
            })
        );
    }

    #[test]
    #[should_panic(expected = "message and transaction context out of sync, fatal")]
    fn test_new_panic() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();
        let extra_account = Pubkey::new_unique();

        let message = Message {
            account_keys: vec![account, program, other_account, extra_account],
            header: MessageHeader::default(),
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![0],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![2],
                    data: vec![],
                },
            ],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message =
            SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()));

        let transaction_accounts = vec![
            (account, AccountSharedData::default()),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20, 2);
        let _result =
            TransactionAccountStateInfo::new_pre_exec(&context, &sanitized_message, &rent, true);
    }

    #[test]
    fn test_post_exec_unchanged_size_allows_pre_balance() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();

        let data_len = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full - 5;
        // account must be both initialized and have a sub-exempt balance
        assert!(pre_balance > 0);

        let sanitized_message = sanitized_msg_for(program, account, other_account);
        let tx_accounts = vec![
            (
                account,
                AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context = TransactionContext::new(tx_accounts, rent.clone(), 20, 20, 2);
        let pre =
            TransactionAccountStateInfo::new_pre_exec(&context, &sanitized_message, &rent, true);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            pre[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentExempt)
        );
        assert_eq!(
            post[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentExempt)
        );
    }

    #[test]
    fn test_post_exec_legacy_ignores_pre_balance() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();

        let data_len = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full - 5;
        // account must be both initialized and have a sub-exempt balance
        assert!(pre_balance > 0);

        let sanitized_message = sanitized_msg_for(program, account, other_account);
        let tx_accounts = vec![
            (
                account,
                AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context = TransactionContext::new(tx_accounts, rent.clone(), 20, 20, 2);
        let pre =
            TransactionAccountStateInfo::new_pre_exec(&context, &sanitized_message, &rent, false);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context,
            &sanitized_message,
            &pre,
            &rent,
            false,
        );

        assert_eq!(
            pre[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentPaying {
                data_size: data_len,
                lamports: pre_balance
            })
        );
        assert_eq!(
            post[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentPaying {
                data_size: data_len,
                lamports: pre_balance
            })
        );
        assert!(post[1].info.as_ref().is_none());
    }

    #[test]
    fn test_basic_rent_transition_violation() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();

        let data_len = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full - 5;
        // account must be both initialized and have a sub-exempt balance
        assert!(pre_balance > 0);

        let sanitized_message = sanitized_msg_for(program, account, other_account);
        let mut tx_accounts = vec![
            (
                account,
                AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_pre = TransactionContext::new(tx_accounts.clone(), rent.clone(), 20, 20, 2);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &rent,
            true,
        );

        // post: drop balance by 1 (below minimum balance)
        let post_balance = pre_balance - 1;
        assert!(pre_balance > 0);
        tx_accounts[0].1.set_lamports(post_balance);

        let context_post = TransactionContext::new(tx_accounts, rent.clone(), 20, 20, 2);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            pre[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentExempt)
        );
        assert_eq!(
            post[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentPaying {
                data_size: data_len,
                lamports: post_balance
            })
        );

        // verify_changes should flag InsufficientFundsForRent
        let res = verify_changes(&pre, &post, &context_post);
        assert_eq!(
            res.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }

    #[test]
    fn test_post_exec_size_changed_requires_current_rent_min() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();

        let pre_len = 32;
        // size increase prevents the post-exec rent-exempt relaxation from applying,
        // so the account will be subject to the full rent-exempt minimum during balance
        // checks.
        let post_len = 256;
        let pre_min = rent.minimum_balance(pre_len);
        let pre_balance = pre_min - 1;
        // account must be both initialized and have a sub-exempt balance
        assert!(pre_balance > 0);

        let sanitized_message = sanitized_msg_for(program, account, other_account);

        let tx_accounts_pre = vec![
            (
                account,
                AccountSharedData::new(pre_balance, pre_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_pre = TransactionContext::new(tx_accounts_pre, rent.clone(), 20, 20, 2);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &rent,
            true,
        );

        // post: size increased; balance unchanged => should require full min for new size
        let tx_accounts_post = vec![
            (
                account,
                AccountSharedData::new(pre_balance, post_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_post = TransactionContext::new(tx_accounts_post, rent.clone(), 20, 20, 2);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            pre[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentExempt)
        );
        assert_eq!(
            post[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentPaying {
                data_size: post_len,
                lamports: pre_balance
            })
        );
        let res = verify_changes(&pre, &post, &context_post);
        assert_eq!(
            res.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );

        // If the account is topped up to the new rent-exempt minimum after the
        // resize, the same transition becomes valid.
        let post_min = rent.minimum_balance(post_len);
        let tx_accounts_post = vec![
            (
                account,
                AccountSharedData::new(post_min, post_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_post = TransactionContext::new(tx_accounts_post, rent.clone(), 20, 20, 2);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            post[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentExempt)
        );
        assert!(verify_changes(&pre, &post, &context_post).is_ok());
    }

    #[test]
    fn test_post_exec_size_shrunk_does_not_require_balance_adjustment() {
        let pre_rent = Rent::default();
        let post_rent = Rent {
            lamports_per_byte: pre_rent.lamports_per_byte * 9,
            ..Rent::default()
        };

        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();

        let pre_len = 256;
        let post_len = pre_len / 8; // smaller size, but rent increased after creation
        let pre_balance = pre_rent.minimum_balance(pre_len);
        let post_min = post_rent.minimum_balance(post_len);
        assert!(post_min > pre_balance);

        let sanitized_message = sanitized_msg_for(program, account, other_account);

        let tx_accounts_pre = vec![
            (
                account,
                AccountSharedData::new(pre_balance, pre_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_pre = TransactionContext::new(tx_accounts_pre, pre_rent.clone(), 20, 20, 2);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &pre_rent,
            true,
        );

        // post: size decreased; rent increased => should still not require top-up
        let tx_accounts_post = vec![
            (
                account,
                AccountSharedData::new(pre_balance, post_len, &Pubkey::default()),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_post = TransactionContext::new(tx_accounts_post, post_rent.clone(), 20, 20, 2);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &post_rent,
            true,
        );

        assert_eq!(
            pre[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentExempt)
        );
        assert_eq!(
            post[0].info.as_ref().unwrap().rent_state,
            RentState::RentExempt
        );
        let res = verify_changes(&pre, &post, &context_post);
        assert!(res.is_ok());
    }

    #[test]
    fn test_post_exec_owner_changed_requires_full_min() {
        let rent = Rent::default();
        let account = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other_account = Pubkey::new_unique();
        let owner_pre = Pubkey::new_unique();
        let owner_post = Pubkey::new_unique();

        let data_len = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full - 5;
        // account must be both initialized and have a sub-exempt balance
        assert!(pre_balance > 0);

        let sanitized_message = sanitized_msg_for(program, account, other_account);

        let tx_accounts_pre = vec![
            (
                account,
                AccountSharedData::new(pre_balance, data_len, &owner_pre),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_pre = TransactionContext::new(tx_accounts_pre, rent.clone(), 20, 20, 2);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &rent,
            true,
        );

        // post: owner changed; balance/size unchanged => should require full min
        let tx_accounts_post = vec![
            (
                account,
                AccountSharedData::new(pre_balance, data_len, &owner_post),
            ),
            (program, AccountSharedData::default()),
            (other_account, AccountSharedData::default()),
        ];
        let context_post = TransactionContext::new(tx_accounts_post, rent.clone(), 20, 20, 2);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            pre[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentExempt)
        );
        assert_eq!(
            post[0].info.as_ref().map(|info| &info.rent_state),
            Some(&RentState::RentPaying {
                data_size: data_len,
                lamports: pre_balance
            })
        );
        let res = verify_changes(&pre, &post, &context_post);
        assert_eq!(
            res.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }

    #[test]
    fn test_verify_changes() {
        // mostly a sanity check for the plumbing since transitions_allowed enforces
        // the specific transition rules.
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let owner = Pubkey::default();
        let pre_state_infos = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                balance: 0,
                data_size: 0,
                owner,
            }),
        }];
        let post_rent_states = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                balance: 0,
                data_size: 0,
                owner,
            }),
        }];

        let transaction_accounts = vec![
            (key1, AccountSharedData::default()),
            (key2, AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 2);

        let result = verify_changes(&pre_state_infos, &post_rent_states, &context);
        assert!(result.is_ok());

        let pre_state_infos = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                balance: 0,
                data_size: 0,
                owner,
            }),
        }];
        let post_rent_states = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::RentPaying {
                    data_size: 2,
                    lamports: 5,
                },
                balance: 0,
                data_size: 0,
                owner,
            }),
        }];

        let transaction_accounts = vec![
            (key1, AccountSharedData::default()),
            (key2, AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 2);
        let result = verify_changes(&pre_state_infos, &post_rent_states, &context);
        assert_eq!(
            result.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }

    #[test]
    fn test_get_uninitialized_accounts_size_with_deleted_accounts() {
        let mut post_state_infos: Vec<TransactionAccountStateInfo> =
            repeat_with(|| TransactionAccountStateInfo {
                info: Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    balance: 0,
                    data_size: 50,
                    owner: Pubkey::new_unique(),
                }),
            })
            .take(3)
            .collect();

        // add non-deleted account to ensure only uninitialized accounts are counted
        post_state_infos.push(TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::RentExempt,
                balance: 100,
                data_size: 50,
                owner: Pubkey::new_unique(),
            }),
        });

        // 3 deleted accounts should contribute 3 * (50) = 150 to the count
        assert_eq!(get_uninitialized_accounts_size(&post_state_infos), 150);
    }
}
