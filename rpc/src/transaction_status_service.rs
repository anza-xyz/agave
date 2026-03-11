//! The `TransactionStatusService` receives executed transactions and creates
//! transaction metadata objects to persist into the Blockstore and optionally
//! broadcast over geyser. The service also records block metadata for any
//! frozen banks it receives.

use {
    crate::transaction_notifier_interface::TransactionNotifierArc,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    itertools::izip,
    solana_clock::Slot,
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError},
        blockstore_processor::{TransactionStatusBatch, TransactionStatusMessage},
    },
    solana_runtime::{
        bank::{Bank, KeyedRewardsAndNumPartitions},
        dependency_tracker::DependencyTracker,
    },
    solana_svm::transaction_commit_result::CommittedTransaction,
    solana_transaction_status::{
        Reward, RewardsAndNumPartitions, TransactionStatusMeta, extract_and_fmt_memos,
        map_inner_instructions,
    },
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
enum Error {
    #[error("blockstore operation failed: {0}")]
    Blockstore(#[from] BlockstoreError),

    #[error("received nonfrozen bank: {0}")]
    NonFrozenBank(Slot),
}
type Result<T> = std::result::Result<T, Error>;

// Used when draining and shutting down TSS in unit tests.
#[cfg(feature = "dev-context-only-utils")]
const TSS_TEST_QUIESCE_NUM_RETRIES: usize = 100;
#[cfg(feature = "dev-context-only-utils")]
const TSS_TEST_QUIESCE_SLEEP_TIME_MS: u64 = 50;

pub struct TransactionStatusService {
    thread_hdl: JoinHandle<()>,
    #[cfg(feature = "dev-context-only-utils")]
    transaction_status_receiver: Receiver<TransactionStatusMessage>,
}

impl TransactionStatusService {
    const SERVICE_NAME: &str = "TransactionStatusService";

    pub fn new(
        transaction_status_receiver: Receiver<TransactionStatusMessage>,
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        enable_rpc_transaction_history: bool,
        transaction_notifier: Option<TransactionNotifierArc>,
        blockstore: Arc<Blockstore>,
        enable_extended_tx_metadata_storage: bool,
        depenency_tracker: Option<Arc<DependencyTracker>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solTxStatusWrtr".to_string())
            .spawn({
                let transaction_status_receiver = transaction_status_receiver.clone();
                move || {
                    info!("{} has started", Self::SERVICE_NAME);
                    loop {
                        if exit.load(Ordering::Relaxed) {
                            break;
                        }

                        let message = match transaction_status_receiver
                            .recv_timeout(Duration::from_secs(1))
                        {
                            Ok(message) => message,
                            Err(err @ RecvTimeoutError::Disconnected) => {
                                info!("{} is stopping because: {err}", Self::SERVICE_NAME);
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                continue;
                            }
                        };

                        match Self::write_transaction_status_batch(
                            message,
                            &max_complete_transaction_status_slot,
                            enable_rpc_transaction_history,
                            transaction_notifier.clone(),
                            &blockstore,
                            enable_extended_tx_metadata_storage,
                            depenency_tracker.clone(),
                        ) {
                            Ok(_) => {}
                            Err(err) => {
                                error!("{} is stopping because: {err}", Self::SERVICE_NAME);
                                exit.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                    info!("{} has stopped", Self::SERVICE_NAME);
                }
            })
            .unwrap();
        Self {
            thread_hdl,
            #[cfg(feature = "dev-context-only-utils")]
            transaction_status_receiver,
        }
    }

    fn write_transaction_status_batch(
        transaction_status_message: TransactionStatusMessage,
        max_complete_transaction_status_slot: &Arc<AtomicU64>,
        enable_rpc_transaction_history: bool,
        transaction_notifier: Option<TransactionNotifierArc>,
        blockstore: &Blockstore,
        enable_extended_tx_metadata_storage: bool,
        dependency_tracker: Option<Arc<DependencyTracker>>,
    ) -> Result<()> {
        match transaction_status_message {
            TransactionStatusMessage::Batch((
                TransactionStatusBatch {
                    slot,
                    transactions,
                    commit_results,
                    balances,
                    token_balances,
                    costs,
                    transaction_indexes,
                },
                work_id,
            )) => {
                let mut status_and_memos_batch = if enable_rpc_transaction_history {
                    Some(blockstore.get_write_batch()?)
                } else {
                    None
                };

                for (
                    transaction,
                    commit_result,
                    pre_balances,
                    post_balances,
                    pre_token_balances,
                    post_token_balances,
                    cost,
                    transaction_index,
                ) in izip!(
                    transactions,
                    commit_results,
                    balances.pre_balances,
                    balances.post_balances,
                    token_balances.pre_token_balances,
                    token_balances.post_token_balances,
                    costs,
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
                        ..
                    } = committed_tx;

                    let fee = fee_details.total_fee();
                    let inner_instructions = inner_instructions.map(|inner_instructions| {
                        map_inner_instructions(inner_instructions).collect()
                    });

                    let pre_token_balances = Some(pre_token_balances);
                    let post_token_balances = Some(post_token_balances);
                    let rewards = Some(vec![]);
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
                        cost_units: cost,
                    };

                    if let Some(transaction_notifier) = transaction_notifier.as_ref() {
                        let is_vote = transaction.is_simple_vote_transaction();
                        let message_hash = transaction.message_hash();
                        let signature = transaction.signature();
                        let transaction = transaction.to_versioned_transaction();
                        transaction_notifier.notify_transaction(
                            slot,
                            transaction_index,
                            signature,
                            message_hash,
                            is_vote,
                            &transaction_status_meta,
                            &transaction,
                        );
                    }

                    if !(enable_extended_tx_metadata_storage || transaction_notifier.is_some()) {
                        transaction_status_meta.log_messages.take();
                        transaction_status_meta.inner_instructions.take();
                        transaction_status_meta.return_data.take();
                    }

                    if let Some(batch) = status_and_memos_batch.as_mut() {
                        if let Some(memos) = extract_and_fmt_memos(transaction.message()) {
                            blockstore.add_transaction_memos_to_batch(
                                transaction.signature(),
                                slot,
                                memos,
                                batch,
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
                            batch,
                        )?;
                    }
                }

                if let Some(batch) = status_and_memos_batch {
                    blockstore.write_batch(batch)?;
                }

                if let Some(dependency_tracker) = dependency_tracker.as_ref() {
                    if let Some(work_id) = work_id {
                        dependency_tracker.mark_this_and_all_previous_work_processed(work_id);
                    }
                }
            }
            TransactionStatusMessage::Freeze(bank) => {
                if !bank.is_frozen() {
                    return Err(Error::NonFrozenBank(bank.slot()));
                }
                Self::write_block_meta(&bank, blockstore)?;
                max_complete_transaction_status_slot.fetch_max(bank.slot(), Ordering::SeqCst);
            }
        }
        Ok(())
    }

    fn write_block_meta(bank: &Bank, blockstore: &Blockstore) -> Result<()> {
        let slot = bank.slot();

        blockstore.set_block_time(slot, bank.clock().unix_timestamp)?;
        blockstore.set_block_height(slot, bank.block_height())?;

        let rewards = bank.get_rewards_and_num_partitions();
        if rewards.should_record() {
            let KeyedRewardsAndNumPartitions {
                keyed_rewards,
                num_partitions,
            } = rewards;
            let commission_rate_in_basis_points = bank
                .feature_set
                .is_active(&agave_feature_set::commission_rate_in_basis_points::id());
            let rewards = keyed_rewards
                .into_iter()
                .map(|(pubkey, reward_info)| Reward {
                    pubkey: pubkey.to_string(),
                    lamports: reward_info.lamports,
                    post_balance: reward_info.post_balance,
                    reward_type: Some(reward_info.reward_type),
                    commission: if commission_rate_in_basis_points {
                        None
                    } else {
                        reward_info.commission_bps.map(|bps| (bps / 100) as u8)
                    },
                    commission_bps: if commission_rate_in_basis_points {
                        reward_info.commission_bps
                    } else {
                        None
                    },
                })
                .collect();
            let blockstore_rewards = RewardsAndNumPartitions {
                rewards,
                num_partitions,
            };

            blockstore.write_rewards(slot, blockstore_rewards)?;
        }

        Ok(())
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }

    // Many tests expect all messages to be handled. Wait for the message
    // queue to drain out before signaling the service to exit.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn quiesce_and_join_for_tests(self, exit: Arc<AtomicBool>) {
        for _ in 0..TSS_TEST_QUIESCE_NUM_RETRIES {
            if self.transaction_status_receiver.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(TSS_TEST_QUIESCE_SLEEP_TIME_MS));
        }
        assert!(
            self.transaction_status_receiver.is_empty(),
            "TransactionStatusService timed out before processing all queued up messages."
        );
        exit.store(true, Ordering::Relaxed);
        self.join().unwrap();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::transaction_notifier_interface::TransactionNotifier,
        agave_reserved_account_keys::ReservedAccountKeys,
        crossbeam_channel::unbounded,
        dashmap::DashMap,
        solana_account::state_traits::StateMut,
        solana_account_decoder::{
            parse_account_data::SplTokenAdditionalDataV2, parse_token::token_amount_to_ui_amount_v3,
        },
        solana_clock::Slot,
        solana_fee_structure::FeeDetails,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{genesis_utils::create_genesis_config, get_tmp_ledger_path_auto_delete},
        solana_message::SimpleAddressLoader,
        solana_nonce::{self as nonce, state::DurableNonce},
        solana_nonce_account as nonce_account,
        solana_pubkey::Pubkey,
        solana_runtime::bank::{Bank, TransactionBalancesSet},
        solana_signature::Signature,
        solana_signer::Signer,
        solana_svm::transaction_execution_result::TransactionLoadedAccountsStats,
        solana_system_transaction as system_transaction,
        solana_transaction::{
            Transaction,
            sanitized::{MessageHash, SanitizedTransaction},
            versioned::VersionedTransaction,
        },
        solana_transaction_status::{
            TransactionStatusMeta, TransactionTokenBalance,
            token_balances::TransactionTokenBalancesSet,
        },
        std::sync::{Arc, atomic::AtomicBool},
    };

    #[derive(Eq, Hash, PartialEq)]
    struct TestNotifierKey {
        slot: Slot,
        transaction_index: usize,
        message_hash: Hash,
    }

    struct TestNotification {
        _meta: TransactionStatusMeta,
        transaction: VersionedTransaction,
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
            _signature: &Signature,
            message_hash: &Hash,
            _is_vote: bool,
            transaction_status_meta: &TransactionStatusMeta,
            transaction: &VersionedTransaction,
        ) {
            self.notifications.insert(
                TestNotifierKey {
                    slot,
                    transaction_index,
                    message_hash: *message_hash,
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
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

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

        let mut nonce_account = nonce_account::create_account(1).into_inner();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_from_array([42u8; 32]));
        let data = nonce::state::Data::new(Pubkey::from([1u8; 32]), durable_nonce, 42);
        nonce_account
            .set_state(&nonce::versions::Versions::new(
                nonce::state::State::Initialized(data),
            ))
            .unwrap();

        let commit_result = Ok(CommittedTransaction {
            status: Ok(()),
            log_messages: None,
            inner_instructions: None,
            return_data: None,
            executed_units: 0,
            fee_details: FeeDetails::default(),
            loaded_account_stats: TransactionLoadedAccountsStats::default(),
            fee_payer_post_balance: 0,
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
            ui_token_amount: token_amount_to_ui_amount_v3(
                42,
                &SplTokenAdditionalDataV2::with_decimals(2),
            ),
            owner: owner.clone(),
            program_id: token_program_id.clone(),
        };

        let post_token_balance = TransactionTokenBalance {
            account_index: 0,
            mint: Pubkey::new_unique().to_string(),
            ui_token_amount: token_amount_to_ui_amount_v3(
                58,
                &SplTokenAdditionalDataV2::with_decimals(2),
            ),
            owner,
            program_id: token_program_id,
        };

        let token_balances = TransactionTokenBalancesSet {
            pre_token_balances: vec![vec![pre_token_balance]],
            post_token_balances: vec![vec![post_token_balance]],
        };

        let slot = bank.slot();
        let message_hash = *transaction.message_hash();
        let transaction_index: usize = bank.transaction_count().try_into().unwrap();
        let transaction_status_batch = TransactionStatusBatch {
            slot,
            transactions: vec![transaction],
            commit_results: vec![commit_result],
            balances,
            token_balances,
            costs: vec![Some(123)],
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
            None, // No work dependency tracker
            exit.clone(),
        );

        transaction_status_sender
            .send(TransactionStatusMessage::Batch((
                transaction_status_batch,
                None, /* No work id */
            )))
            .unwrap();

        transaction_status_service.quiesce_and_join_for_tests(exit);
        assert_eq!(test_notifier.notifications.len(), 1);
        let key = TestNotifierKey {
            slot,
            transaction_index,
            message_hash,
        };
        assert!(test_notifier.notifications.contains_key(&key));

        let result = test_notifier.notifications.get(&key).unwrap();
        assert_eq!(
            expected_transaction.signature(),
            result.transaction.signatures.first().unwrap()
        );
    }

    #[test]
    fn test_batch_transaction_status_and_memos() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

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
            loaded_account_stats: TransactionLoadedAccountsStats::default(),
            fee_payer_post_balance: 0,
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
            costs: vec![Some(123), Some(456)],
            transaction_indexes: vec![transaction_index1, transaction_index2],
        };

        let test_notifier = Arc::new(TestTransactionNotifier::new());

        let dependency_tracker = Arc::new(DependencyTracker::default());
        let exit = Arc::new(AtomicBool::new(false));
        let transaction_status_service = TransactionStatusService::new(
            transaction_status_receiver,
            Arc::new(AtomicU64::default()),
            true,
            Some(test_notifier.clone()),
            blockstore,
            false,
            Some(dependency_tracker.clone()),
            exit.clone(),
        );
        let work_id = 345;
        transaction_status_sender
            .send(TransactionStatusMessage::Batch((
                transaction_status_batch,
                Some(work_id),
            )))
            .unwrap();
        transaction_status_service.quiesce_and_join_for_tests(exit);
        assert_eq!(test_notifier.notifications.len(), 2);

        let key1 = TestNotifierKey {
            slot,
            transaction_index: transaction_index1,
            message_hash: *expected_transaction1.message_hash(),
        };
        let key2 = TestNotifierKey {
            slot,
            transaction_index: transaction_index2,
            message_hash: *expected_transaction2.message_hash(),
        };

        assert!(test_notifier.notifications.contains_key(&key1));
        assert!(test_notifier.notifications.contains_key(&key2));

        let result1 = test_notifier.notifications.get(&key1).unwrap();
        let result2 = test_notifier.notifications.get(&key2).unwrap();

        assert_eq!(
            expected_transaction1.signature(),
            result1.transaction.signatures.first().unwrap()
        );
        assert_eq!(
            expected_transaction1.message_hash(),
            &result1.transaction.message.hash(),
        );
        assert_eq!(
            expected_transaction2.signature(),
            result2.transaction.signatures.first().unwrap()
        );
        assert_eq!(
            expected_transaction2.message_hash(),
            &result2.transaction.message.hash(),
        );
    }
}
