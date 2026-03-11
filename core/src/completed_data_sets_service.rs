//! [`CompletedDataSetsService`] is a hub, that runs different operations when a "completed data
//! set", also known as a [`Vec<Entry>`], is received by the validator.
//!
//! Currently, `WindowService` sends [`CompletedDataSetInfo`]s via a `completed_sets_receiver`
//! provided to the [`CompletedDataSetsService`].

use {
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender},
    solana_entry::entry::Entry,
    solana_ledger::{
        blockstore::{Blockstore, CompletedDataSetInfo},
        deshred_transaction_notifier_interface::{
            DeshredTransactionNotifier, DeshredTransactionNotifierArc,
        },
    },
    solana_measure::measure::Measure,
    solana_message::{VersionedMessage, v0::LoadedAddresses},
    solana_metrics::*,
    solana_rpc::{max_slots::MaxSlots, rpc_subscriptions::RpcSubscriptions},
    solana_runtime::bank_forks::BankForks,
    solana_signature::Signature,
    solana_svm_transaction::message_address_table_lookup::SVMMessageAddressTableLookup,
    solana_transaction::{
        simple_vote_transaction_checker::is_simple_vote_transaction_impl,
        versioned::VersionedTransaction,
    },
    std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub type CompletedDataSetsReceiver = Receiver<Vec<CompletedDataSetInfo>>;
pub type CompletedDataSetsSender = Sender<Vec<CompletedDataSetInfo>>;

/// Check if a versioned transaction is a simple vote transaction.
/// This avoids cloning by extracting the required data directly.
fn is_simple_vote_transaction(tx: &VersionedTransaction) -> bool {
    let is_legacy = matches!(&tx.message, VersionedMessage::Legacy(_));
    let instruction_programs = tx
        .message
        .instructions()
        .iter()
        .filter_map(|ix| {
            tx.message
                .static_account_keys()
                .get(ix.program_id_index as usize)
        });
    is_simple_vote_transaction_impl(&tx.signatures, is_legacy, instruction_programs)
}

/// Load addresses from address lookup tables for a versioned transaction.
/// Returns None for legacy transactions or if address resolution fails.
/// Takes a Bank reference to avoid repeated lock acquisition.
fn load_transaction_addresses(
    tx: &VersionedTransaction,
    bank: &solana_runtime::bank::Bank,
) -> Option<LoadedAddresses> {
    let address_table_lookups = tx.message.address_table_lookups()?;
    if address_table_lookups.is_empty() {
        return None;
    }

    bank.load_addresses_from_ref(
        address_table_lookups
            .iter()
            .map(SVMMessageAddressTableLookup::from),
    )
    .ok()
    .map(|(addresses, _deactivation_slot)| addresses)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DeshredBatchStats {
    total_lut_load_us: u64,
    total_notify_us: u64,
    total_transactions: u64,
    total_entries: u64,
    total_data_sets: u64,
    lut_transactions: u64,
}

pub struct CompletedDataSetsService {
    thread_hdl: JoinHandle<()>,
}

impl CompletedDataSetsService {
    pub fn new(
        completed_sets_receiver: CompletedDataSetsReceiver,
        blockstore: Arc<Blockstore>,
        rpc_subscriptions: Arc<RpcSubscriptions>,
        deshred_transaction_notifier: Option<DeshredTransactionNotifierArc>,
        exit: Arc<AtomicBool>,
        max_slots: Arc<MaxSlots>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solComplDataSet".to_string())
            .spawn(move || {
                info!("CompletedDataSetsService has started");
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(RecvTimeoutError::Disconnected) = Self::recv_completed_data_sets(
                        &completed_sets_receiver,
                        &blockstore,
                        &rpc_subscriptions,
                        &deshred_transaction_notifier,
                        &max_slots,
                        &bank_forks,
                    ) {
                        break;
                    }
                }
                info!("CompletedDataSetsService has stopped");
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn recv_completed_data_sets(
        completed_sets_receiver: &CompletedDataSetsReceiver,
        blockstore: &Blockstore,
        rpc_subscriptions: &RpcSubscriptions,
        deshred_transaction_notifier: &Option<DeshredTransactionNotifierArc>,
        max_slots: &Arc<MaxSlots>,
        bank_forks: &RwLock<BankForks>,
    ) -> Result<(), RecvTimeoutError> {
        const RECV_TIMEOUT: Duration = Duration::from_secs(1);
        let first_completed_data_sets = completed_sets_receiver.recv_timeout(RECV_TIMEOUT)?;
        let root_bank = deshred_transaction_notifier.as_ref().map(|_| {
            // Best-effort ALT resolution uses the rooted bank to avoid surfacing fork-local state.
            bank_forks.read().unwrap().root_bank()
        });
        let mut batch_measure = Measure::start("deshred_geyser_batch");
        let mut stats = DeshredBatchStats::default();

        let slots = std::iter::once(first_completed_data_sets)
            .chain(completed_sets_receiver.try_iter())
            .flatten()
            .map(|completed_data_set_info| {
                let CompletedDataSetInfo { slot, indices } = completed_data_set_info;
                match blockstore.get_entries_in_data_block(slot, indices, /*slot_meta:*/ None) {
                    Ok(entries) => {
                        Self::notify_deshred_transactions_for_completed_data_set(
                            slot,
                            &entries,
                            deshred_transaction_notifier.as_deref(),
                            root_bank.as_deref(),
                            &mut stats,
                        );

                        let transactions = Self::get_transaction_signatures(entries);
                        if !transactions.is_empty() {
                            rpc_subscriptions.notify_signatures_received((slot, transactions));
                        }
                    }
                    Err(e) => warn!("completed-data-set-service deserialize error: {e:?}"),
                }
                slot
            });

        if let Some(slot) = slots.max() {
            max_slots.shred_insert.fetch_max(slot, Ordering::Relaxed);
        }

        batch_measure.stop();

        if deshred_transaction_notifier.is_some() {
            let avg_notify_us = stats
                .total_notify_us
                .checked_div(stats.total_transactions)
                .unwrap_or(0);
            datapoint_info!(
                "deshred_geyser_timing",
                ("batch_total_us", batch_measure.as_us() as i64, i64),
                ("notify_total_us", stats.total_notify_us as i64, i64),
                ("lut_load_total_us", stats.total_lut_load_us as i64, i64),
                ("transactions_count", stats.total_transactions as i64, i64),
                ("lut_transactions_count", stats.lut_transactions as i64, i64),
                ("entries_count", stats.total_entries as i64, i64),
                ("data_sets_count", stats.total_data_sets as i64, i64),
                ("avg_notify_us", avg_notify_us as i64, i64),
            );
        }

        Ok(())
    }

    fn notify_deshred_transactions_for_completed_data_set(
        slot: u64,
        entries: &[Entry],
        deshred_transaction_notifier: Option<&(dyn DeshredTransactionNotifier + Send + Sync)>,
        root_bank: Option<&solana_runtime::bank::Bank>,
        stats: &mut DeshredBatchStats,
    ) {
        let Some(notifier) = deshred_transaction_notifier else {
            return;
        };

        stats.total_data_sets += 1;
        stats.total_entries += entries.len() as u64;

        for entry in entries {
            for tx in &entry.transactions {
                let Some(signature) = tx.signatures.first() else {
                    continue;
                };

                stats.total_transactions += 1;
                let is_vote = is_simple_vote_transaction(tx);

                let mut lut_measure = Measure::start("load_lut");
                let loaded_addresses =
                    root_bank.and_then(|bank| load_transaction_addresses(tx, bank));
                lut_measure.stop();

                if loaded_addresses.is_some() {
                    stats.lut_transactions += 1;
                    stats.total_lut_load_us += lut_measure.as_us();
                }

                let mut notify_measure = Measure::start("notify_deshred");
                notifier.notify_deshred_transaction(
                    slot,
                    signature,
                    is_vote,
                    tx,
                    loaded_addresses.as_ref(),
                );
                notify_measure.stop();
                stats.total_notify_us += notify_measure.as_us();
            }
        }
    }

    fn get_transaction_signatures(entries: Vec<Entry>) -> Vec<Signature> {
        entries
            .into_iter()
            .flat_map(|e| {
                e.transactions
                    .into_iter()
                    .filter_map(|mut t| t.signatures.drain(..).next())
            })
            .collect::<Vec<Signature>>()
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        crossbeam_channel::bounded,
        solana_entry::entry::next_versioned_entry,
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_instruction::Instruction,
        solana_keypair::Keypair,
        solana_ledger::{blockstore, blockstore::Blockstore, get_tmp_ledger_path_auto_delete},
        solana_message::{
            Message, VersionedMessage,
            v0::{self, LoadedAddresses},
        },
        solana_pubkey::Pubkey,
        solana_rpc::{
            max_slots::MaxSlots,
            optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
            rpc_subscriptions::RpcSubscriptions,
        },
        solana_runtime::{bank::Bank, bank_forks::BankForks, commitment::BlockCommitmentCache},
        solana_signature::Signature,
        solana_signer::Signer,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
        std::sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct DeshredNotification {
        slot: u64,
        signature: Signature,
        is_vote: bool,
        transaction: VersionedTransaction,
        loaded_addresses: Option<LoadedAddresses>,
    }

    #[derive(Default)]
    struct TestDeshredTransactionNotifier {
        notifications: Mutex<Vec<DeshredNotification>>,
    }

    impl DeshredTransactionNotifier for TestDeshredTransactionNotifier {
        fn notify_deshred_transaction(
            &self,
            slot: u64,
            signature: &Signature,
            is_vote: bool,
            transaction: &VersionedTransaction,
            loaded_addresses: Option<&LoadedAddresses>,
        ) {
            self.notifications
                .lock()
                .unwrap()
                .push(DeshredNotification {
                    slot,
                    signature: *signature,
                    is_vote,
                    transaction: transaction.clone(),
                    loaded_addresses: loaded_addresses.cloned(),
                });
        }
    }

    fn legacy_transaction(instruction: Instruction) -> VersionedTransaction {
        let keypair = Keypair::new();
        VersionedTransaction::try_new(
            VersionedMessage::Legacy(Message::new(&[instruction], Some(&keypair.pubkey()))),
            &[&keypair],
        )
        .unwrap()
    }

    fn versioned_v0_transaction(instruction: Instruction) -> VersionedTransaction {
        let keypair = Keypair::new();
        let message =
            v0::Message::try_compile(&keypair.pubkey(), &[instruction], &[], Hash::default())
                .unwrap();
        VersionedTransaction::try_new(VersionedMessage::V0(message), &[&keypair]).unwrap()
    }

    #[test]
    fn test_zero_signatures() {
        let tx = Transaction::new_with_payer(&[], None);
        let entries = vec![Entry::new(&Hash::default(), 1, vec![tx])];
        let signatures = CompletedDataSetsService::get_transaction_signatures(entries);
        assert!(signatures.is_empty());
    }

    #[test]
    fn test_multi_signatures() {
        let kp = Keypair::new();
        let tx =
            Transaction::new_signed_with_payer(&[], Some(&kp.pubkey()), &[&kp], Hash::default());
        let entries = vec![Entry::new(&Hash::default(), 1, vec![tx.clone()])];
        let signatures = CompletedDataSetsService::get_transaction_signatures(entries);
        assert_eq!(signatures.len(), 1);

        let entries = vec![
            Entry::new(&Hash::default(), 1, vec![tx.clone(), tx.clone()]),
            Entry::new(&Hash::default(), 1, vec![tx]),
        ];
        let signatures = CompletedDataSetsService::get_transaction_signatures(entries);
        assert_eq!(signatures.len(), 3);
    }

    #[test]
    fn test_is_simple_vote_transaction_paths() {
        let vote_instruction =
            Instruction::new_with_bytes(solana_sdk_ids::vote::ID, &[], Vec::new());
        let non_vote_instruction =
            Instruction::new_with_bytes(Pubkey::new_unique(), &[], Vec::new());

        assert!(is_simple_vote_transaction(&legacy_transaction(
            vote_instruction
        )));
        assert!(!is_simple_vote_transaction(&legacy_transaction(
            non_vote_instruction
        )));
        assert!(!is_simple_vote_transaction(&versioned_v0_transaction(
            Instruction::new_with_bytes(solana_sdk_ids::vote::ID, &[], Vec::new()),
        )));
    }

    #[test]
    fn test_load_transaction_addresses_returns_none_without_lookups() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());

        assert!(
            load_transaction_addresses(
                &legacy_transaction(Instruction::new_with_bytes(
                    Pubkey::new_unique(),
                    &[],
                    Vec::new(),
                )),
                &bank,
            )
            .is_none()
        );
        assert!(
            load_transaction_addresses(
                &versioned_v0_transaction(Instruction::new_with_bytes(
                    Pubkey::new_unique(),
                    &[],
                    Vec::new(),
                )),
                &bank,
            )
            .is_none()
        );
    }

    #[test]
    fn test_notify_deshred_transactions_for_completed_data_set() {
        let notifier = TestDeshredTransactionNotifier::default();
        let legacy_vote_tx = legacy_transaction(Instruction::new_with_bytes(
            solana_sdk_ids::vote::ID,
            &[],
            Vec::new(),
        ));
        let legacy_non_vote_tx = legacy_transaction(Instruction::new_with_bytes(
            Pubkey::new_unique(),
            &[],
            Vec::new(),
        ));
        let unsigned_tx = VersionedTransaction {
            signatures: vec![],
            message: VersionedMessage::Legacy(Message::new(&[], None)),
        };
        let entries = vec![
            next_versioned_entry(&Hash::default(), 1, vec![legacy_vote_tx.clone()]),
            next_versioned_entry(
                &Hash::new_unique(),
                1,
                vec![legacy_non_vote_tx.clone(), unsigned_tx],
            ),
        ];
        let mut stats = DeshredBatchStats::default();

        CompletedDataSetsService::notify_deshred_transactions_for_completed_data_set(
            42,
            &entries,
            Some(&notifier),
            None,
            &mut stats,
        );

        let notifications = notifier.notifications.lock().unwrap().clone();
        assert_eq!(notifications.len(), 2);
        assert_eq!(notifications[0].slot, 42);
        assert_eq!(notifications[0].signature, legacy_vote_tx.signatures[0]);
        assert!(notifications[0].is_vote);
        assert_eq!(notifications[1].signature, legacy_non_vote_tx.signatures[0]);
        assert!(!notifications[1].is_vote);
        assert!(
            notifications
                .iter()
                .all(|notification| notification.loaded_addresses.is_none())
        );
        assert_eq!(stats.total_transactions, 2);
        assert_eq!(stats.total_entries, 2);
        assert_eq!(stats.total_data_sets, 1);
        assert_eq!(stats.total_lut_load_us, 0);
        assert_eq!(stats.lut_transactions, 0);
    }

    #[test]
    fn test_recv_completed_data_sets_notifies_and_updates_max_slot() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&GenesisConfig::default()));
        let rpc_subscriptions = RpcSubscriptions::new_for_tests_with_blockstore(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::default()),
            blockstore.clone(),
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::new_for_tests())),
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks),
        );
        let max_slots = Arc::new(MaxSlots::default());
        let test_notifier = Arc::new(TestDeshredTransactionNotifier::default());
        let notifier = Some(test_notifier.clone() as DeshredTransactionNotifierArc);
        let (sender, receiver) = bounded(1);
        let entries = vec![next_versioned_entry(
            &Hash::default(),
            1,
            vec![legacy_transaction(Instruction::new_with_bytes(
                Pubkey::new_unique(),
                &[],
                Vec::new(),
            ))],
        )];
        let shreds = blockstore::entries_to_test_shreds(&entries, 11, 10, true, 0);
        let completed_data_sets = blockstore.insert_shreds(shreds, None, true).unwrap();
        assert_eq!(completed_data_sets.len(), 1);
        sender.send(completed_data_sets).unwrap();

        CompletedDataSetsService::recv_completed_data_sets(
            &receiver,
            &blockstore,
            &rpc_subscriptions,
            &notifier,
            &max_slots,
            &bank_forks,
        )
        .unwrap();

        let notifications = test_notifier.notifications.lock().unwrap().clone();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].slot, 11);
        assert_eq!(max_slots.shred_insert.load(Ordering::Relaxed), 11);
    }
}
