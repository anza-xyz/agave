use {
    crate::rent_calculator::{check_rent_state, get_account_rent_state, RentState},
    solana_account::ReadableAccount,
    solana_rent::Rent,
    solana_sdk_ids::native_loader,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_context::{IndexOfAccount, TransactionContext},
    solana_transaction_error::TransactionResult as Result,
    std::cmp::min,
};

#[derive(PartialEq, Debug)]
pub struct WritableTransactionAccountStateInfo {
    rent_state: RentState,
    balance: u64,
    data_size: usize,
}

pub type TransactionAccountStateInfo = Option<WritableTransactionAccountStateInfo>; // None: readonly account

pub(crate) fn new_pre_exec(
    transaction_context: &TransactionContext,
    message: &impl SVMMessage,
) -> Vec<TransactionAccountStateInfo> {
    (0..message.account_keys().len())
        .map(|i| {
            if message.is_writable(i) {
                if let Ok(account) = transaction_context
                    .accounts()
                    .try_borrow(i as IndexOfAccount)
                {
                    // Native programs appear to be RentPaying because they carry low lamport
                    // balances; however they will never be loaded as writable
                    debug_assert!(!native_loader::check_id(account.owner()));

                    // RentState::RentPaying is no longer allowed
                    let state = if account.lamports() == 0 {
                        RentState::Uninitialized
                    } else {
                        RentState::RentExempt
                    };

                    Some(WritableTransactionAccountStateInfo {
                        rent_state: state,
                        balance: account.lamports(),
                        data_size: account.data().len(),
                    })
                } else {
                    panic!("message and transaction context out of sync, fatal");
                }
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn new_post_exec(
    transaction_context: &TransactionContext,
    pre_exec_state_infos: &[TransactionAccountStateInfo],
    message: &impl SVMMessage,
    rent: &Rent,
) -> Vec<TransactionAccountStateInfo> {
    debug_assert_eq!(pre_exec_state_infos.len(), message.account_keys().len());

    // zip pre_exec state with accounts
    (0..message.account_keys().len())
        .zip(pre_exec_state_infos)
        .map(|(i, pre_exec_state_info)| {
            if message.is_writable(i) {
                if let Ok(account) = transaction_context
                    .accounts()
                    .try_borrow(i as IndexOfAccount)
                {
                    // Native programs appear to be RentPaying because they carry low lamport
                    // balances; however they will never be loaded as writable
                    debug_assert!(!native_loader::check_id(account.owner()));

                    // the same account MUST be present and marked writable in both pre and post exec
                    debug_assert!(pre_exec_state_info.is_some());
                    let pre_exec_state_info = pre_exec_state_info.as_ref().unwrap();

                    // if the account size increased or account was created in this transaction,
                    // do standard rent exempt check
                    // otherwise, pre-exec balance can also be used as the minimum balance
                    let mut min_balance = rent.minimum_balance(account.data().len());
                    if pre_exec_state_info.rent_state != RentState::Uninitialized
                        && account.data().len() <= pre_exec_state_info.data_size
                    {
                        min_balance = min(pre_exec_state_info.balance, min_balance);
                    }

                    Some(WritableTransactionAccountStateInfo {
                        rent_state: get_account_rent_state(
                            account.lamports(),
                            account.data().len(),
                            min_balance,
                        ),
                        balance: account.lamports(),
                        data_size: account.data().len(),
                    })
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn verify_changes(
    pre_state_infos: &[TransactionAccountStateInfo],
    post_state_infos: &[TransactionAccountStateInfo],
    transaction_context: &TransactionContext,
) -> Result<()> {
    for (i, state_info) in pre_state_infos.iter().zip(post_state_infos).enumerate() {
        if let (Some(pre_state_info), Some(post_state_info)) = state_info {
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

pub(crate) fn get_account_state_growth_delta(
    pre: &[TransactionAccountStateInfo],
    post: &[TransactionAccountStateInfo],
) -> i64 {
    pre.iter().zip(post).fold(0i64, |sum, (pre_opt, post_opt)| {
        match (pre_opt.as_ref(), post_opt.as_ref()) {
            (Some(pre), Some(post)) => {
                sum + match (&pre.rent_state, &post.rent_state) {
                    (RentState::Uninitialized, RentState::RentExempt) => {
                        post.data_size as i64 + solana_rent::ACCOUNT_STORAGE_OVERHEAD as i64
                    } // created
                    (RentState::RentExempt, RentState::Uninitialized) => {
                        -(pre.data_size as i64 + solana_rent::ACCOUNT_STORAGE_OVERHEAD as i64)
                    } // deleted
                    _ => post.data_size as i64 - pre.data_size as i64,
                }
            }
            _ => sum, // skip if either side is None
        }
    })
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_account::{AccountSharedData, WritableAccount},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            compiled_instruction::CompiledInstruction, LegacyMessage, Message, MessageHeader,
            SanitizedMessage,
        },
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_signer::Signer,
        solana_transaction_context::TransactionContext,
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
        TransactionContext::new(accounts, rent.clone(), 20, 20)
    }

    fn is_rent_exempt(v: &TransactionAccountStateInfo) -> bool {
        matches!(
            v,
            Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::RentExempt,
                ..
            })
        )
    }

    fn is_rent_paying(v: &TransactionAccountStateInfo) -> bool {
        matches!(
            v,
            Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::RentPaying { .. },
                ..
            })
        )
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

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20);
        let result = new_pre_exec(&context, &sanitized_message);
        assert_eq!(
            result,
            vec![
                Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    balance: 0,
                    data_size: 0,
                }),
                None,
                Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    balance: 0,
                    data_size: 0,
                }),
            ]
        );

        // no post-exec in this test
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

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20);
        let _result = new_pre_exec(&context, &sanitized_message);
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
        let pre = new_pre_exec(&context, &sanitized_message);
        let post = new_post_exec(&context, &pre, &sanitized_message, &rent);

        // account index 0 in message is key2; expect RentExempt due to grace
        assert!(is_rent_exempt(&post[0]));
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
        let context_pre = TransactionContext::new(tx_accounts.clone(), rent.clone(), 20, 20);
        let pre = new_pre_exec(&context_pre, &sanitized_message);

        // post: drop balance by 1 (below minimum balance)
        let post_balance = pre_balance.saturating_sub(1);
        tx_accounts[0].1.set_lamports(post_balance);

        let context_post = TransactionContext::new(tx_accounts, rent.clone(), 20, 20);
        let post = new_post_exec(&context_post, &pre, &sanitized_message, &rent);

        assert!(is_rent_paying(&post[0]));

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
        let context_pre = TransactionContext::new(tx_accounts_pre, rent.clone(), 20, 20);
        let pre = new_pre_exec(&context_pre, &sanitized_message);

        // post: size increased; balance unchanged => should require full min for new size
        let tx_accounts_post = accounts_key2_first(
            key1.pubkey(),
            key2.pubkey(),
            key3.pubkey(),
            AccountSharedData::new(pre_balance, post_len, &Pubkey::default()),
        );
        let context_post = TransactionContext::new(tx_accounts_post, rent.clone(), 20, 20);
        let post = new_post_exec(&context_post, &pre, &sanitized_message, &rent);

        assert!(is_rent_paying(&post[0]));
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
        let pre_rent_state = vec![
            Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                balance: 0,
                data_size: 0,
            }),
            Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                balance: 0,
                data_size: 0,
            }),
        ];
        let post_rent_state = vec![Some(WritableTransactionAccountStateInfo {
            rent_state: RentState::Uninitialized,
            balance: 0,
            data_size: 0,
        })];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20);

        let result = verify_changes(&pre_rent_state, &post_rent_state, &context);
        assert!(result.is_ok());

        let pre_rent_state = vec![Some(WritableTransactionAccountStateInfo {
            rent_state: RentState::Uninitialized,
            balance: 0,
            data_size: 0,
        })];
        let post_rent_state = vec![Some(WritableTransactionAccountStateInfo {
            rent_state: RentState::RentPaying {
                data_size: 2,
                lamports: 5,
            },
            balance: 0,
            data_size: 0,
        })];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20);
        let result = verify_changes(&pre_rent_state, &post_rent_state, &context);
        assert_eq!(
            result.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }
}
