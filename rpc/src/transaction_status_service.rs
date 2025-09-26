//! The `TransactionStatusService` receives executed transactions and creates
//! transaction metadata objects to persist into the Blockstore and optionally
//! broadcast over geyser. The service also records block metadata for any
//! frozen banks it receives.

use {
    crate::{
        slot_completion_tracker::{
            SlotCompletionCallback, SlotCompletionCallbackImpl, SlotCompletionTracker,
        },
        transaction_notifier_interface::TransactionNotifierArc,
    },
    crossbeam_channel::{Receiver, TryRecvError},
    itertools::izip,
    rayon::{
        iter::{IntoParallelIterator, ParallelIterator},
        ThreadPoolBuilder,
    },
    solana_clock::Slot,
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError},
        blockstore_processor::TransactionStatusMessage,
    },
    solana_runtime::{
        bank::{Bank, KeyedRewardsAndNumPartitions},
        dependency_tracker::DependencyTracker,
    },
    solana_svm::transaction_commit_result::CommittedTransaction,
    solana_transaction_status::{
        extract_and_fmt_memos, map_inner_instructions, Reward, RewardsAndNumPartitions,
        TransactionStatusMeta,
    },
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        thread::{self, sleep, Builder, JoinHandle},
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

const NUM_TSS_WORKER_THREADS: usize = 16;

const MESSAGE_BATCH_SIZE: usize = 40;

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

                    let worker_thread_pool = ThreadPoolBuilder::new()
                        .num_threads(NUM_TSS_WORKER_THREADS)
                        .build()
                        .unwrap();

                    let slot_tracker = Arc::new(SlotCompletionTracker::new(Arc::clone(
                        &max_complete_transaction_status_slot,
                    )));

                    let slot_completion_callback =
                        Arc::new(SlotCompletionCallbackImpl::new(Arc::clone(&slot_tracker)));

                    let mut pending_message: Option<TransactionStatusMessage> = None;

                    loop {
                        if exit.load(Ordering::Relaxed) {
                            break;
                        }

                        if let Err(err) = Self::write_pending_bank_freeze_message(
                            &mut pending_message,
                            &blockstore,
                            &max_complete_transaction_status_slot,
                        ) {
                            error!("{} is stopping because: {err}", Self::SERVICE_NAME);
                            exit.store(true, Ordering::Relaxed);
                            break;
                        }

                        let mut messages = match Self::aggregate_messages(
                            &transaction_status_receiver,
                            &mut pending_message,
                        ) {
                            Ok(msgs) => {
                                if !msgs.is_empty() {
                                    msgs
                                } else {
                                    sleep(Duration::from_millis(5));
                                    continue;
                                }
                            }
                            Err(err) => {
                                error!("{} is stopping because: {err}", Self::SERVICE_NAME);
                                exit.store(true, Ordering::Relaxed);
                                break;
                            }
                        };

                        if messages.is_empty() {
                            sleep(Duration::from_millis(5));
                            continue;
                        }

                        let chunk_size = messages.len().div_ceil(NUM_TSS_WORKER_THREADS);
                        // Drain messages into non-overlapping chunks
                        let mut chunks = Vec::new();
                        let mut drain = messages.drain(..);
                        while !drain.as_slice().is_empty() {
                            let chunk: Vec<_> = drain.by_ref().take(chunk_size).collect();
                            if chunk.is_empty() {
                                break;
                            }
                            chunks.push(chunk);
                        }
                        // Now process each chunk in parallel
                        if !chunks.is_empty() {
                            let slot = match &chunks[0][0] {
                                TransactionStatusMessage::Batch((batch, _)) => batch.slot,
                                TransactionStatusMessage::Freeze(bank) => bank.slot(),
                            };
                            slot_tracker.start_batch(slot);
                            worker_thread_pool.install(|| {
                                chunks.into_par_iter().for_each(|chunk| {
                                    if let Err(err) = Self::write_transaction_status_batch(
                                        chunk,
                                        enable_rpc_transaction_history,
                                        transaction_notifier.clone(),
                                        &blockstore,
                                        enable_extended_tx_metadata_storage,
                                        depenency_tracker.clone(),
                                        slot_completion_callback.clone(),
                                    ) {
                                        error!("{} is stopping because: {err}", Self::SERVICE_NAME);
                                        exit.store(true, Ordering::Relaxed);
                                    }
                                });
                            });
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
        messages: Vec<TransactionStatusMessage>,
        enable_rpc_transaction_history: bool,
        transaction_notifier: Option<TransactionNotifierArc>,
        blockstore: &Blockstore,
        enable_extended_tx_metadata_storage: bool,
        dependency_tracker: Option<Arc<DependencyTracker>>,
        slot_completion_callback: Arc<dyn SlotCompletionCallback>,
    ) -> Result<()> {
        let mut slot = None;
        let mut work_ids = Vec::new();

        // Collect all zipped transaction batch data for this slot
        let mut zipped_batches = Vec::new();

        for message in messages {
            match message {
                TransactionStatusMessage::Batch((batch, work_id)) => {
                    if slot.is_none() {
                        slot = Some(batch.slot);
                    }
                    let zipped = izip!(
                        batch.transactions,
                        batch.commit_results,
                        batch.balances.pre_balances,
                        batch.balances.post_balances,
                        batch.token_balances.pre_token_balances,
                        batch.token_balances.post_token_balances,
                        batch.costs,
                        batch.transaction_indexes,
                    );
                    zipped_batches.extend(zipped);

                    if let Some(id) = work_id {
                        work_ids.push(id);
                    }
                }
                _ => {
                    unreachable!(
                        "Only TransactionStatusMessage::Batch is expected here. Freeze messages \
                         should have been handled by this point."
                    );
                }
            }
        }

        let slot = slot.unwrap();
        let mut status_and_memos_batch = blockstore.get_write_batch()?;

        for (
            transaction,
            commit_result,
            pre_balances,
            post_balances,
            pre_token_balances,
            post_token_balances,
            cost,
            transaction_index,
        ) in zipped_batches
        {
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
            let inner_instructions = inner_instructions
                .map(|inner_instructions| map_inner_instructions(inner_instructions).collect());

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

        //fixme: add this line to right place for freeze and batch
        slot_completion_callback.on_slot_complete(slot);

        if let Some(dependency_tracker) = dependency_tracker.as_ref() {
            for work_id in work_ids {
                dependency_tracker.mark_this_and_all_previous_work_processed(work_id);
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
            let rewards = keyed_rewards
                .into_iter()
                .map(|(pubkey, reward_info)| Reward {
                    pubkey: pubkey.to_string(),
                    lamports: reward_info.lamports,
                    post_balance: reward_info.post_balance,
                    reward_type: Some(reward_info.reward_type),
                    commission: reward_info.commission,
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

    fn aggregate_messages(
        receiver: &Receiver<TransactionStatusMessage>,
        pending_message: &mut Option<TransactionStatusMessage>,
    ) -> Result<Vec<TransactionStatusMessage>> {
        let mut batch_messages = Vec::with_capacity(MESSAGE_BATCH_SIZE);
        let mut current_slot: Option<Slot> = None;

        // Use pending message first
        if let Some(message) = pending_message.take() {
            debug_assert!(!matches!(message, TransactionStatusMessage::Freeze(_)));
            let slot = Self::get_slot(&message);
            current_slot = Some(slot);
            batch_messages.push(message);
        }

        loop {
            let message = match receiver.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(Error::Blockstore(BlockstoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "receiver disconnected",
                    ))));
                }
            };

            let slot = Self::get_slot(&message);
            match current_slot {
                Some(current) if slot != current => {
                    *pending_message = Some(message);
                    break;
                }
                None => current_slot = Some(slot),
                _ => {}
            }

            match message {
                TransactionStatusMessage::Freeze(_) => {
                    *pending_message = Some(message);
                    break;
                }
                _ => {
                    batch_messages.push(message);
                }
            }
        }

        Ok(batch_messages)
    }

    fn write_pending_bank_freeze_message(
        pending_message: &mut Option<TransactionStatusMessage>,
        blockstore: &Blockstore,
        max_complete_transaction_status_slot: &Arc<AtomicU64>,
    ) -> Result<()> {
        if let Some(TransactionStatusMessage::Freeze(bank)) = pending_message.take() {
            if !bank.is_frozen() {
                return Err(Error::NonFrozenBank(bank.slot()));
            }
            Self::write_block_meta(&bank, blockstore)?;
            max_complete_transaction_status_slot.fetch_max(bank.slot(), Ordering::SeqCst);
        }
        Ok(())
    }

    fn get_slot(message: &TransactionStatusMessage) -> Slot {
        match message {
            TransactionStatusMessage::Batch((batch, _)) => batch.slot,
            TransactionStatusMessage::Freeze(bank) => bank.slot(),
        }
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
        solana_ledger::{
            blockstore_processor::TransactionStatusBatch, genesis_utils::create_genesis_config,
            get_tmp_ledger_path_auto_delete,
        },
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
            sanitized::{MessageHash, SanitizedTransaction},
            versioned::VersionedTransaction,
            Transaction,
        },
        solana_transaction_status::{
            token_balances::TransactionTokenBalancesSet, TransactionStatusMeta,
            TransactionTokenBalance,
        },
        std::sync::{atomic::AtomicBool, Arc},
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
