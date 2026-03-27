use {
    crate::rent_calculator::{RentState, check_rent_state, get_account_rent_state},
    solana_account::ReadableAccount,
    solana_rent::Rent,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_context::{IndexOfAccount, transaction::TransactionContext},
    solana_transaction_error::TransactionResult as Result,
};

#[derive(PartialEq, Debug)]
pub(crate) struct TransactionAccountStateInfo {
    info: Option<WritableTransactionAccountStateInfo>, // None: readonly account
}

#[derive(PartialEq, Debug)]
pub struct WritableTransactionAccountStateInfo {
    rent_state: RentState,
    data_size: usize,
}

impl TransactionAccountStateInfo {
    pub(crate) fn new(
        transaction_context: &TransactionContext,
        message: &impl SVMMessage,
        rent: &Rent,
    ) -> Vec<Self> {
        (0..message.account_keys().len())
            .map(|i| {
                let info = if message.is_writable(i) {
                    let state = if let Ok(account) = transaction_context
                        .accounts()
                        .try_borrow(i as IndexOfAccount)
                    {
                        let balance = account.lamports();
                        let data_size = account.data().len();
                        let rent_state = get_account_rent_state(rent, balance, data_size);
                        Some(WritableTransactionAccountStateInfo {
                            rent_state,
                            data_size,
                        })
                    } else {
                        None
                    };
                    debug_assert!(
                        state.is_some(),
                        "message and transaction context out of sync, fatal"
                    );
                    state
                } else {
                    None
                };
                Self { info }
            })
            .collect()
    }

    pub(crate) fn verify_changes(
        pre_state_infos: &[Self],
        post_state_infos: &[Self],
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
}

// Returns account data size delta for a given transaction execution
pub(crate) fn get_account_data_len_delta(
    post: &[TransactionAccountStateInfo],
    accounts_resize_delta: i64,
) -> i64 {
    // accounts_resize_delta accounts doesn't account for deleted accounts, so the overall
    // state delta is computed by subtracting the data size of every deleted account.
    post.iter()
        .fold(accounts_resize_delta, |data_size_delta, post_info| {
            if let Some(post) = &post_info.info {
                match &post.rent_state {
                    // deleted: post-exec data size is used because `post_size - pre_size` is already
                    // accounted for in accounts_resize_delta.
                    RentState::Uninitialized => data_size_delta - post.data_size as i64,
                    // existing account or new account creation
                    _ => data_size_delta,
                }
            } else {
                // None indicates non write-locked accounts -> no change
                data_size_delta
            }
        })
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_account::AccountSharedData,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
            compiled_instruction::CompiledInstruction,
        },
        solana_rent::Rent,
        solana_signer::Signer,
        solana_transaction_context::transaction::TransactionContext,
        solana_transaction_error::TransactionError,
        std::collections::HashSet,
    };

    #[test]
    fn test_new() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key4.pubkey()],
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
        let result = TransactionAccountStateInfo::new(&context, &sanitized_message, &rent);
        assert_eq!(
            result,
            vec![
                TransactionAccountStateInfo {
                    info: Some(WritableTransactionAccountStateInfo {
                        rent_state: RentState::Uninitialized,
                        data_size: 0,
                    })
                },
                TransactionAccountStateInfo { info: None },
                TransactionAccountStateInfo {
                    info: Some(WritableTransactionAccountStateInfo {
                        rent_state: RentState::Uninitialized,
                        data_size: 0,
                    })
                }
            ]
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
        let _result = TransactionAccountStateInfo::new(&context, &sanitized_message, &rent);
    }

    #[test]
    fn test_verify_changes() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let pre_rent_state = vec![
            TransactionAccountStateInfo {
                info: Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    data_size: 0,
                }),
            },
            TransactionAccountStateInfo {
                info: Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    data_size: 0,
                }),
            },
        ];
        let post_rent_state = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                data_size: 0,
            }),
        }];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 1);

        let result = TransactionAccountStateInfo::verify_changes(
            &pre_rent_state,
            &post_rent_state,
            &context,
        );
        assert!(result.is_ok());

        let pre_rent_state = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::Uninitialized,
                data_size: 0,
            }),
        }];
        let post_rent_state = vec![TransactionAccountStateInfo {
            info: Some(WritableTransactionAccountStateInfo {
                rent_state: RentState::RentPaying {
                    data_size: 2,
                    lamports: 5,
                },
                data_size: 2,
            }),
        }];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 1);
        let result = TransactionAccountStateInfo::verify_changes(
            &pre_rent_state,
            &post_rent_state,
            &context,
        );
        assert_eq!(
            result.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }

    #[test]
    fn test_get_account_data_len_delta_with_deleted_account() {
        let post_state_infos = vec![
            TransactionAccountStateInfo {
                info: Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    data_size: 50,
                }),
            },
            TransactionAccountStateInfo {
                info: Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    data_size: 50,
                }),
            },
            TransactionAccountStateInfo {
                info: Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::Uninitialized,
                    data_size: 50,
                }),
            },
            TransactionAccountStateInfo {
                info: Some(WritableTransactionAccountStateInfo {
                    rent_state: RentState::RentExempt,
                    data_size: 50,
                }),
            },
        ];

        let accounts_resize_delta = 0;
        let delta = get_account_data_len_delta(&post_state_infos, accounts_resize_delta);

        // 3 deleted accounts should contribute 3 * (-50) = -150 to the delta
        assert_eq!(delta, -150);
    }
}
