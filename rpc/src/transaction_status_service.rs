use {
    crate::transaction_notifier_interface::TransactionNotifierArc,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    itertools::izip,
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError},
        blockstore_processor::{TransactionStatusBatch, TransactionStatusMessage},
    },
    solana_svm::transaction_commit_result::CommittedTransaction,
    solana_transaction_status::{
        extract_and_fmt_memos, map_inner_instructions, Reward, TransactionStatusMeta,
    },
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

const NUM_TSS_WORKER_THREADS: usize = 4;

pub struct TransactionStatusService {
    thread_handles: Vec<JoinHandle<()>>,
}

impl TransactionStatusService {
    pub fn new(
        write_transaction_status_receiver: Receiver<TransactionStatusMessage>,
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        enable_rpc_transaction_history: bool,
        transaction_notifier: Option<TransactionNotifierArc>,
        blockstore: Arc<Blockstore>,
        enable_extended_tx_metadata_storage: bool,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let mut thread_handles = vec![];
        let write_transaction_status_receiver =
            Arc::new(Mutex::new(write_transaction_status_receiver));

        for worker in 0..NUM_TSS_WORKER_THREADS {
            let write_transaction_status_receiver = Arc::clone(&write_transaction_status_receiver);
            let max_complete_transaction_status_slot =
                Arc::clone(&max_complete_transaction_status_slot);
            let transaction_notifier = transaction_notifier.clone();
            let blockstore = Arc::clone(&blockstore);
            let exit = Arc::clone(&exit);

            let thread_hdl = Builder::new()
                .name(format!("solTxStatusWrtr-{}", worker))
                .spawn(move || {
                    info!("TransactionStatusService worker {worker} has started");
                    loop {
                        if exit.load(Ordering::Relaxed) {
                                break;
                            }

                        let message = {
                            let tss_receiver = write_transaction_status_receiver.lock().unwrap();
                            match tss_receiver.recv_timeout(Duration::from_secs(1)) {
                                Ok(message) => message,
                                Err(RecvTimeoutError::Disconnected) => {
                                    break;
                                }
                                Err(RecvTimeoutError::Timeout) => {
                                    continue;
                                }
                            }
                        };

                        match Self::write_transaction_status_batch(
                            message,
                            &max_complete_transaction_status_slot,
                            enable_rpc_transaction_history,
                            transaction_notifier.clone(),
                            &blockstore,
                            enable_extended_tx_metadata_storage,
                        ) {
                            Ok(_) => {}
                            Err(err) => {
                                error!("TransactionStatusService worker {worker} stopping due to error: {err}");
                                exit.store(true, Ordering::Relaxed);
                                break;
                            }
                            }
                        }
                    info!("TransactionStatusService worker {worker} has stopped");
                })
                .unwrap();

            thread_handles.push(thread_hdl);
        }
        Self { thread_handles }
    }

    fn write_transaction_status_batch(
        transaction_status_message: TransactionStatusMessage,
        max_complete_transaction_status_slot: &Arc<AtomicU64>,
        enable_rpc_transaction_history: bool,
        transaction_notifier: Option<TransactionNotifierArc>,
        blockstore: &Blockstore,
        enable_extended_tx_metadata_storage: bool,
    ) -> Result<(), BlockstoreError> {
        match transaction_status_message {
            TransactionStatusMessage::Batch(TransactionStatusBatch {
                slot,
                transactions,
                commit_results,
                balances,
                token_balances,
                transaction_indexes,
            }) => {
                let mut status_and_memos_batch = blockstore.get_write_batch()?;

                for (
                    transaction,
                    commit_result,
                    pre_balances,
                    post_balances,
                    pre_token_balances,
                    post_token_balances,
                    transaction_index,
                ) in izip!(
                    transactions,
                    commit_results,
                    balances.pre_balances,
                    balances.post_balances,
                    token_balances.pre_token_balances,
                    token_balances.post_token_balances,
                    transaction_indexes,
                ) {
                    let Ok(committed_tx) = commit_result else {
                        continue;
                    };

                    let CommittedTransaction {
                        status,
                        log_messages,
                        inner_instructions,
                        return_data,
                        executed_units,
                        fee_details,
                        rent_debits,
                        ..
                    } = committed_tx;

                    let fee = fee_details.total_fee();
                    let inner_instructions = inner_instructions.map(|inner_instructions| {
                        map_inner_instructions(inner_instructions).collect()
                    });

                    let pre_token_balances = Some(pre_token_balances);
                    let post_token_balances = Some(post_token_balances);
                    let rewards = Some(
                        rent_debits
                            .into_unordered_rewards_iter()
                            .map(|(pubkey, reward_info)| Reward {
                                pubkey: pubkey.to_string(),
                                lamports: reward_info.lamports,
                                post_balance: reward_info.post_balance,
                                reward_type: Some(reward_info.reward_type),
                                commission: reward_info.commission,
                            })
                            .collect(),
                    );
                    let loaded_addresses = transaction.get_loaded_addresses();
                    let mut transaction_status_meta = TransactionStatusMeta {
                        status,
                        fee,
                        pre_balances,
                        post_balances,
                        inner_instructions,
                        log_messages,
                        pre_token_balances,
                        post_token_balances,
                        rewards,
                        loaded_addresses,
                        return_data,
                        compute_units_consumed: Some(executed_units),
                    };

                    if let Some(transaction_notifier) = transaction_notifier.as_ref() {
                        transaction_notifier.notify_transaction(
                            slot,
                            transaction_index,
                            transaction.signature(),
                            &transaction_status_meta,
                            &transaction,
                        );
                    }

                    if !(enable_extended_tx_metadata_storage || transaction_notifier.is_some()) {
                        transaction_status_meta.log_messages.take();
                        transaction_status_meta.inner_instructions.take();
                        transaction_status_meta.return_data.take();
                    }

                    if enable_rpc_transaction_history {
                        if let Some(memos) = extract_and_fmt_memos(transaction.message()) {
                            blockstore.add_transaction_memos_to_batch(
                                transaction.signature(),
                                slot,
                                memos,
                                &mut status_and_memos_batch,
                            )?;
                        }

                        let message = transaction.message();
                        let keys_with_writable = message
                            .account_keys()
                            .iter()
                            .enumerate()
                            .map(|(index, key)| (key, message.is_writable(index)));

                        blockstore.add_transaction_status_to_batch(
                            slot,
                            *transaction.signature(),
                            keys_with_writable,
                            transaction_status_meta,
                            transaction_index,
                            &mut status_and_memos_batch,
                        )?;
                    }
                }

                if enable_rpc_transaction_history {
                    blockstore.write_batch(status_and_memos_batch)?;
                }
            }
            TransactionStatusMessage::Freeze(slot) => {
                max_complete_transaction_status_slot.fetch_max(slot, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    pub fn join(self) -> thread::Result<()> {
        let mut result = Ok(());
        for handle in self.thread_handles {
            if let Err(err) = handle.join() {
                result = Err(err);
            }
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::transaction_notifier_interface::TransactionNotifier,
        crossbeam_channel::unbounded,
        dashmap::DashMap,
        solana_account_decoder::{
            parse_account_data::SplTokenAdditionalData, parse_token::token_amount_to_ui_amount_v2,
        },
        solana_ledger::{genesis_utils::create_genesis_config, get_tmp_ledger_path_auto_delete},
        solana_runtime::bank::{Bank, TransactionBalancesSet},
        solana_sdk::{
            account_utils::StateMut,
            clock::Slot,
            fee::FeeDetails,
            hash::Hash,
            nonce::{self, state::DurableNonce},
            nonce_account,
            pubkey::Pubkey,
            rent_debits::RentDebits,
            reserved_account_keys::ReservedAccountKeys,
            signature::{Keypair, Signature, Signer},
            system_transaction,
            transaction::{
                MessageHash, SanitizedTransaction, SimpleAddressLoader, Transaction,
                VersionedTransaction,
            },
        },
        solana_svm::transaction_execution_result::TransactionLoadedAccountsStats,
        solana_transaction_status::{
            token_balances::TransactionTokenBalancesSet, TransactionStatusMeta,
            TransactionTokenBalance,
        },
        std::{
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            },
            thread::sleep,
            time::Duration,
        },
    };

    #[derive(Eq, Hash, PartialEq)]
    struct TestNotifierKey {
        slot: Slot,
        transaction_index: usize,
        signature: Signature,
    }

    struct TestNotification {
        _meta: TransactionStatusMeta,
        transaction: SanitizedTransaction,
    }

    struct TestTransactionNotifier {
        notifications: DashMap<TestNotifierKey, TestNotification>,
    }

    impl TestTransactionNotifier {
        pub fn new() -> Self {
            Self {
                notifications: DashMap::default(),
            }
        }
    }

    impl TransactionNotifier for TestTransactionNotifier {
        fn notify_transaction(
            &self,
            slot: Slot,
            transaction_index: usize,
            signature: &Signature,
            transaction_status_meta: &TransactionStatusMeta,
            transaction: &SanitizedTransaction,
        ) {
            self.notifications.insert(
                TestNotifierKey {
                    slot,
                    transaction_index,
                    signature: *signature,
                },
                TestNotification {
                    _meta: transaction_status_meta.clone(),
                    transaction: transaction.clone(),
                },
            );
        }
    }

    fn build_test_transaction_legacy() -> Transaction {
        let keypair1 = Keypair::new();
        let pubkey1 = keypair1.pubkey();
        let zero = Hash::default();
        system_transaction::transfer(&keypair1, &pubkey1, 42, zero)
    }

    #[test]
    fn test_notify_transaction() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let (transaction_status_sender, transaction_status_receiver) = unbounded();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let blockstore = Arc::new(blockstore);

        let transaction = build_test_transaction_legacy();
        let transaction = VersionedTransaction::from(transaction);
        let transaction = SanitizedTransaction::try_create(
            transaction,
            MessageHash::Compute,
            None,
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let expected_transaction = transaction.clone();
        let pubkey = Pubkey::new_unique();

        let mut nonce_account = nonce_account::create_account(1).into_inner();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_from_array([42u8; 32]));
        let data = nonce::state::Data::new(Pubkey::from([1u8; 32]), durable_nonce, 42);
        nonce_account
            .set_state(&nonce::state::Versions::new(nonce::State::Initialized(
                data,
            )))
            .unwrap();

        let mut rent_debits = RentDebits::default();
        rent_debits.insert(&pubkey, 123, 456);

        let commit_result = Ok(CommittedTransaction {
            status: Ok(()),
            log_messages: None,
            inner_instructions: None,
            return_data: None,
            executed_units: 0,
            fee_details: FeeDetails::default(),
            rent_debits,
            loaded_account_stats: TransactionLoadedAccountsStats::default(),
        });

        let balances = TransactionBalancesSet {
            pre_balances: vec![vec![123456]],
            post_balances: vec![vec![234567]],
        };

        let owner = Pubkey::new_unique().to_string();
        let token_program_id = Pubkey::new_unique().to_string();
        let pre_token_balance = TransactionTokenBalance {
            account_index: 0,
            mint: Pubkey::new_unique().to_string(),
            ui_token_amount: token_amount_to_ui_amount_v2(
                42,
                &SplTokenAdditionalData::with_decimals(2),
            ),
            owner: owner.clone(),
            program_id: token_program_id.clone(),
        };

        let post_token_balance = TransactionTokenBalance {
            account_index: 0,
            mint: Pubkey::new_unique().to_string(),
            ui_token_amount: token_amount_to_ui_amount_v2(
                58,
                &SplTokenAdditionalData::with_decimals(2),
            ),
            owner,
            program_id: token_program_id,
        };

        let token_balances = TransactionTokenBalancesSet {
            pre_token_balances: vec![vec![pre_token_balance]],
            post_token_balances: vec![vec![post_token_balance]],
        };

        let slot = bank.slot();
        let signature = *transaction.signature();
        let transaction_index: usize = bank.transaction_count().try_into().unwrap();
        let transaction_status_batch = TransactionStatusBatch {
            slot,
            transactions: vec![transaction],
            commit_results: vec![commit_result],
            balances,
            token_balances,
            transaction_indexes: vec![transaction_index],
        };

        let test_notifier = Arc::new(TestTransactionNotifier::new());

        let exit = Arc::new(AtomicBool::new(false));
        let transaction_status_service = TransactionStatusService::new(
            transaction_status_receiver,
            Arc::new(AtomicU64::default()),
            false,
            Some(test_notifier.clone()),
            blockstore,
            false,
            exit.clone(),
        );

        transaction_status_sender
            .send(TransactionStatusMessage::Batch(transaction_status_batch))
            .unwrap();
        sleep(Duration::from_millis(500));

        exit.store(true, Ordering::Relaxed);
        transaction_status_service.join().unwrap();
        assert_eq!(test_notifier.notifications.len(), 1);
        let key = TestNotifierKey {
            slot,
            transaction_index,
            signature,
        };
        assert!(test_notifier.notifications.contains_key(&key));

        let result = test_notifier.notifications.get(&key).unwrap();
        assert_eq!(
            expected_transaction.signature(),
            result.transaction.signature()
        );
    }

    #[test]
    fn test_batch_transaction_status_and_memos() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let (transaction_status_sender, transaction_status_receiver) = unbounded();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let blockstore = Arc::new(blockstore);

        let transaction1 = build_test_transaction_legacy();
        let transaction1 = VersionedTransaction::from(transaction1);
        let transaction1 = SanitizedTransaction::try_create(
            transaction1,
            MessageHash::Compute,
            None,
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let transaction2 = build_test_transaction_legacy();
        let transaction2 = VersionedTransaction::from(transaction2);
        let transaction2 = SanitizedTransaction::try_create(
            transaction2,
            MessageHash::Compute,
            None,
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let expected_transaction1 = transaction1.clone();
        let expected_transaction2 = transaction2.clone();

        let commit_result = Ok(CommittedTransaction {
            status: Ok(()),
            log_messages: None,
            inner_instructions: None,
            return_data: None,
            executed_units: 0,
            fee_details: FeeDetails::default(),
            rent_debits: RentDebits::default(),
            loaded_account_stats: TransactionLoadedAccountsStats::default(),
        });

        let balances = TransactionBalancesSet {
            pre_balances: vec![vec![123456], vec![234567]],
            post_balances: vec![vec![234567], vec![345678]],
        };

        let token_balances = TransactionTokenBalancesSet {
            pre_token_balances: vec![vec![], vec![]],
            post_token_balances: vec![vec![], vec![]],
        };

        let slot = bank.slot();
        let transaction_index1: usize = bank.transaction_count().try_into().unwrap();
        let transaction_index2: usize = transaction_index1 + 1;

        let transaction_status_batch = TransactionStatusBatch {
            slot,
            transactions: vec![transaction1, transaction2],
            commit_results: vec![commit_result.clone(), commit_result],
            balances: balances.clone(),
            token_balances,
            transaction_indexes: vec![transaction_index1, transaction_index2],
        };

        let test_notifier = Arc::new(TestTransactionNotifier::new());

        let exit = Arc::new(AtomicBool::new(false));
        let transaction_status_service = TransactionStatusService::new(
            transaction_status_receiver,
            Arc::new(AtomicU64::default()),
            true,
            Some(test_notifier.clone()),
            blockstore,
            false,
            exit.clone(),
        );

        transaction_status_sender
            .send(TransactionStatusMessage::Batch(transaction_status_batch))
            .unwrap();
        sleep(Duration::from_millis(5000));

        exit.store(true, Ordering::Relaxed);
        transaction_status_service.join().unwrap();
        assert_eq!(test_notifier.notifications.len(), 2);

        let key1 = TestNotifierKey {
            slot,
            transaction_index: transaction_index1,
            signature: *expected_transaction1.signature(),
        };
        let key2 = TestNotifierKey {
            slot,
            transaction_index: transaction_index2,
            signature: *expected_transaction2.signature(),
        };

        assert!(test_notifier.notifications.contains_key(&key1));
        assert!(test_notifier.notifications.contains_key(&key2));

        let result1 = test_notifier.notifications.get(&key1).unwrap();
        let result2 = test_notifier.notifications.get(&key2).unwrap();

        assert_eq!(
            expected_transaction1.signature(),
            result1.transaction.signature()
        );
        assert_eq!(
            expected_transaction1.message_hash(),
            result1.transaction.message_hash()
        );

        assert_eq!(
            expected_transaction2.signature(),
            result2.transaction.signature()
        );
        assert_eq!(
            expected_transaction2.message_hash(),
            result2.transaction.message_hash()
        );
    }
}
