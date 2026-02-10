use {
    crate::rent_calculator::{RentState, check_rent_state, get_account_rent_state},
    solana_account::ReadableAccount,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_context::{
        IndexOfAccount, transaction::TransactionContext, transaction_accounts::AccountRef,
    },
    solana_transaction_error::TransactionResult as Result,
    std::cmp::min,
};

#[derive(PartialEq, Debug)]
pub struct WritableTransactionAccountStateInfo {
    rent_state: RentState,
    balance: u64,
    data_size: usize,
    owner: Pubkey,
}

#[derive(PartialEq, Debug)]
pub(crate) struct TransactionAccountStateInfo {
    info: Option<WritableTransactionAccountStateInfo>, // None: readonly account
}

fn iter_accounts<'a>(
    transaction_context: &'a TransactionContext<'a>,
    message: &impl SVMMessage,
) -> impl Iterator<Item = Option<AccountRef<'a>>> {
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

impl TransactionAccountStateInfo {
    pub(crate) fn new_pre_exec(
        transaction_context: &TransactionContext,
        message: &impl SVMMessage,
        rent: &Rent,
        relax_post_exec_min_balance_check: bool,
    ) -> Vec<Self> {
        iter_accounts(transaction_context, message)
            .map(|acct_ref| {
                let info = acct_ref.map(|account| {
                    let balance = account.lamports();
                    let data_size = account.data().len();
                    let owner = *account.owner();

                    let rent_state = if relax_post_exec_min_balance_check {
                        // SIMD-0392 enabled. Assume `RentExempt` is impossible.
                        if account.lamports() == 0 {
                            RentState::Uninitialized
                        } else {
                            RentState::RentExempt
                        }
                    } else {
                        get_account_rent_state(balance, data_size, rent.minimum_balance(data_size))
                    };

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
        pre_exec_state_infos: &[Self],
        rent: &Rent,
        relax_post_exec_min_balance_check: bool,
    ) -> Vec<Option<RentState>> {
        debug_assert_eq!(pre_exec_state_infos.len(), message.account_keys().len());

        iter_accounts(transaction_context, message)
            .zip(pre_exec_state_infos)
            .map(|(acct_ref, pre_exec_state_info)| {
                acct_ref.map(|account| {
                    // the same account MUST be present and marked writable in both pre and post exec
                    debug_assert!(
                        pre_exec_state_info.info.is_some(),
                        "message and pre-exec state out of sync, fatal"
                    );

                    let balance = account.lamports();
                    let data_size = account.data().len();
                    let mut min_balance = rent.minimum_balance(data_size);

                    if relax_post_exec_min_balance_check {
                        // SIMD-0392 enabled.
                        // Adjust min_balance according to SIMD-0392.
                        if let Some(pre_exec_info) = &pre_exec_state_info.info {
                            // if the account size increased, account was created in this transaction,
                            // or the owner of the account changed, do standard rent exempt check
                            // otherwise, pre-exec balance can also be used as the minimum balance
                            if pre_exec_info.rent_state != RentState::Uninitialized
                                && data_size <= pre_exec_info.data_size
                                && account.owner() == &pre_exec_info.owner
                            {
                                min_balance = min(pre_exec_info.balance, min_balance);
                            }
                        }
                    }

                    get_account_rent_state(balance, data_size, min_balance)
                })
            })
            .collect()
    }
}

pub(crate) fn verify_changes(
    pre_state_infos: &[TransactionAccountStateInfo],
    post_rent_states: &[Option<RentState>],
    transaction_context: &TransactionContext,
) -> Result<()> {
    for (i, (pre_state_info, post_rent_state)) in
        pre_state_infos.iter().zip(post_rent_states).enumerate()
    {
        if let (Some(pre_state_info), Some(post_rent_state)) =
            (pre_state_info.info.as_ref(), post_rent_state)
        {
            check_rent_state(
                &pre_state_info.rent_state,
                post_rent_state,
                transaction_context,
                i as IndexOfAccount,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_account::{AccountSharedData, WritableAccount},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
            compiled_instruction::CompiledInstruction,
        },
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_signer::Signer,
        solana_transaction_context::transaction::TransactionContext,
        solana_transaction_error::TransactionError,
        std::collections::HashSet,
    };

    // Helpers to reduce duplication in tests
    fn sanitized_msg_for(key1: Pubkey, key2: Pubkey, key4: Pubkey) -> SanitizedMessage {
        let message = Message {
            account_keys: vec![key2, key1, key4],
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

    fn accounts_key2_first(
        key1: Pubkey,
        key2: Pubkey,
        key3: Pubkey,
        acc2: AccountSharedData,
    ) -> Vec<(Pubkey, AccountSharedData)> {
        vec![
            (key2, acc2),
            (key1, AccountSharedData::default()),
            (key3, AccountSharedData::default()),
        ]
    }

    fn ctx_from(accounts: Vec<(Pubkey, AccountSharedData)>, rent: &Rent) -> TransactionContext<'_> {
        TransactionContext::new(accounts, rent.clone(), 20, 20, 1)
    }

    #[test]
    fn test_pre_exec_basics() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
            (key3.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20, 1);
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
                        owner: Pubkey::default(),
                    })
                },
                TransactionAccountStateInfo { info: None },
                TransactionAccountStateInfo {
                    info: Some(WritableTransactionAccountStateInfo {
                        rent_state: RentState::Uninitialized,
                        balance: 0,
                        data_size: 0,
                        owner: Pubkey::default(),
                    })
                },
            ]
        );

        // no post-exec in this test
    }

    #[test]
    fn test_pre_exec_legacy_rent_paying() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let data_len: usize = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full.saturating_sub(1);

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());
        let tx_accounts = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
        );
        let context = ctx_from(tx_accounts, &rent);
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
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key4.pubkey(), key3.pubkey()],
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
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
            (key3.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20, 1);
        let _result =
            TransactionAccountStateInfo::new_pre_exec(&context, &sanitized_message, &rent, true);
    }

    #[test]
    fn test_post_exec_unchanged_size_allows_pre_balance() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let data_len: usize = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full.saturating_sub(5); // less than full rent-exempt, but > 0

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());
        let tx_accounts = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
        );
        let context = ctx_from(tx_accounts, &rent);
        let pre =
            TransactionAccountStateInfo::new_pre_exec(&context, &sanitized_message, &rent, true);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        // account index 0 in message is key2; expect RentExempt due to grace
        assert_eq!(post[0], Some(RentState::RentExempt));
    }

    #[test]
    fn test_post_exec_legacy_ignores_pre_balance() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let data_len: usize = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full.saturating_sub(5);

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());
        let tx_accounts = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
        );
        let context = ctx_from(tx_accounts, &rent);
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
            post[0],
            Some(RentState::RentPaying {
                data_size: data_len,
                lamports: pre_balance
            })
        );
        assert!(post[1].is_none());
    }

    #[test]
    fn test_post_exec_unchanged_size_violation() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let data_len: usize = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full.saturating_sub(5);

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());
        let mut tx_accounts = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, data_len, &Pubkey::default()),
        );
        let context_pre = TransactionContext::new(tx_accounts.clone(), rent.clone(), 20, 20, 1);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &rent,
            true,
        );

        // post: drop balance by 1 (below minimum balance)
        let post_balance = pre_balance.saturating_sub(1);
        tx_accounts[0].1.set_lamports(post_balance);

        let context_post = TransactionContext::new(tx_accounts, rent.clone(), 20, 20, 1);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            post[0],
            Some(RentState::RentPaying {
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
    fn test_post_exec_size_changed_requires_full_min() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let pre_len: usize = 32;
        let post_len: usize = 256; // larger size => larger full min
        let pre_min = rent.minimum_balance(pre_len);
        let pre_balance = pre_min.saturating_sub(1);

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());

        let tx_accounts_pre = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, pre_len, &Pubkey::default()),
        );
        let context_pre = TransactionContext::new(tx_accounts_pre, rent.clone(), 20, 20, 1);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &rent,
            true,
        );

        // post: size increased; balance unchanged => should require full min for new size
        let tx_accounts_post = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, post_len, &Pubkey::default()),
        );
        let context_post = TransactionContext::new(tx_accounts_post, rent.clone(), 20, 20, 1);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            post[0],
            Some(RentState::RentPaying {
                data_size: post_len,
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
    fn test_post_exec_size_shrunk_does_not_require_balance_adjustment() {
        let pre_rent = Rent::default();
        let post_rent = Rent {
            lamports_per_byte_year: 10_000,
            ..Rent::default()
        };

        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let pre_len: usize = 256;
        let post_len: usize = 32; // smaller size, but rent increased after creation
        let pre_balance = pre_rent.minimum_balance(pre_len);
        let post_min = post_rent.minimum_balance(post_len);
        assert!(post_min > pre_balance);

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());

        let tx_accounts_pre = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, pre_len, &Pubkey::default()),
        );
        let context_pre = TransactionContext::new(tx_accounts_pre, pre_rent.clone(), 20, 20, 1);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &pre_rent,
            true,
        );

        // post: size decreased; rent increased => should still not require top-up
        let tx_accounts_post = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, post_len, &Pubkey::default()),
        );
        let context_post = TransactionContext::new(tx_accounts_post, post_rent.clone(), 20, 20, 1);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &post_rent,
            true,
        );

        assert_eq!(post[0], Some(RentState::RentExempt));
        let res = verify_changes(&pre, &post, &context_post);
        assert!(res.is_ok());
    }

    #[test]
    fn test_post_exec_owner_changed_requires_full_min() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();
        let owner_pre = Keypair::new();
        let owner_post = Keypair::new();

        let data_len: usize = 64;
        let min_full = rent.minimum_balance(data_len);
        let pre_balance = min_full.saturating_sub(5);

        let sanitized_message = sanitized_msg_for(key1.pubkey(), key2.pubkey(), key4.pubkey());

        let tx_accounts_pre = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, data_len, &owner_pre.pubkey()),
        );
        let context_pre = TransactionContext::new(tx_accounts_pre, rent.clone(), 20, 20, 1);
        let pre = TransactionAccountStateInfo::new_pre_exec(
            &context_pre,
            &sanitized_message,
            &rent,
            true,
        );

        // post: owner changed; balance/size unchanged => should require full min
        let tx_accounts_post = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, data_len, &owner_post.pubkey()),
        );
        let context_post = TransactionContext::new(tx_accounts_post, rent.clone(), 20, 20, 1);
        let post = TransactionAccountStateInfo::new_post_exec(
            &context_post,
            &sanitized_message,
            &pre,
            &rent,
            true,
        );

        assert_eq!(
            post[0],
            Some(RentState::RentPaying {
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
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let pre_state_infos = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                balance: 0,
                data_size: 0,
                owner: Pubkey::default(),
            }),
        }];
        let post_rent_states = vec![Some(RentState::Uninitialized)];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 1);

        let result = verify_changes(&pre_state_infos, &post_rent_states, &context);
        assert!(result.is_ok());

        let pre_state_infos = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                balance: 0,
                data_size: 0,
                owner: Pubkey::default(),
            }),
        }];
        let post_rent_states = vec![Some(RentState::RentPaying {
            data_size: 2,
            lamports: 5,
        })];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 1);
        let result = verify_changes(&pre_state_infos, &post_rent_states, &context);
        assert_eq!(
            result.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }
}
