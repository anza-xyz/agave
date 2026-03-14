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
        deshred_transaction_notifier_interface::DeshredTransactionNotifierArc,
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
    let (account_keys, instructions) = match &tx.message {
        VersionedMessage::Legacy(msg) => (&msg.account_keys[..], &msg.instructions[..]),
        VersionedMessage::V0(msg) => (&msg.account_keys[..], &msg.instructions[..]),
        VersionedMessage::V1(msg) => (&msg.account_keys[..], &msg.instructions[..]),
    };
    let instruction_programs = instructions
        .iter()
        .filter_map(|ix| account_keys.get(ix.program_id_index as usize));
    is_simple_vote_transaction_impl(&tx.signatures, is_legacy, instruction_programs)
}

/// Load addresses from address lookup tables for a versioned transaction.
/// Returns None for legacy transactions or if address resolution fails.
/// Takes a Bank reference to avoid repeated lock acquisition.
fn load_transaction_addresses(
    tx: &VersionedTransaction,
    bank: &solana_runtime::bank::Bank,
) -> Option<LoadedAddresses> {
    let message = match &tx.message {
        VersionedMessage::V0(msg) => msg,
        // Legacy and V1 transactions don't have address table lookups
        VersionedMessage::Legacy(_) | VersionedMessage::V1(_) => return None,
    };

    if message.address_table_lookups.is_empty() {
        return None;
    }

    bank.load_addresses_from_ref(
        message
            .address_table_lookups
            .iter()
            .map(SVMMessageAddressTableLookup::from),
    )
    .ok()
    .map(|(addresses, _deactivation_slot)| addresses)
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

        let mut batch_measure = Measure::start("deshred_geyser_batch");

        // Get root bank once per batch to minimize lock contention
        let root_bank = deshred_transaction_notifier
            .as_ref()
            .and_then(|_| bank_forks.read().ok())
            .map(|forks| forks.root_bank());

        // Metrics accumulators
        let mut total_lut_load_us: u64 = 0;
        let mut total_notify_us: u64 = 0;
        let mut total_transactions: u64 = 0;
        let mut total_entries: u64 = 0;
        let mut total_data_sets: u64 = 0;
        let mut lut_transactions: u64 = 0;

        let handle_completed_data_set_info =
            |completed_data_set_info: CompletedDataSetInfo,
             total_lut_load_us: &mut u64,
             total_notify_us: &mut u64,
             total_transactions: &mut u64,
             total_entries: &mut u64,
             total_data_sets: &mut u64,
             lut_transactions: &mut u64| {
                let CompletedDataSetInfo { slot, indices } = completed_data_set_info;
                match blockstore.get_entries_in_data_block(slot, indices, /*slot_meta:*/ None) {
                    Ok(entries) => {
                        *total_data_sets += 1;
                        *total_entries += entries.len() as u64;

                        // Notify deshred transactions if notifier is enabled
                        if let Some(notifier) = deshred_transaction_notifier {
                            for entry in entries.iter() {
                                for tx in &entry.transactions {
                                    if let Some(signature) = tx.signatures.first() {
                                        *total_transactions += 1;
                                        let is_vote = is_simple_vote_transaction(tx);

                                        // Measure LUT loading time
                                        let mut lut_measure = Measure::start("load_lut");
                                        let loaded_addresses = root_bank
                                            .as_ref()
                                            .and_then(|bank| load_transaction_addresses(tx, bank));
                                        lut_measure.stop();

                                        if loaded_addresses.is_some() {
                                            *lut_transactions += 1;
                                            *total_lut_load_us += lut_measure.as_us();
                                        }

                                        // Measure notification time
                                        let mut notify_measure = Measure::start("notify_deshred");
                                        notifier.notify_deshred_transaction(
                                            slot,
                                            signature,
                                            is_vote,
                                            tx,
                                            loaded_addresses.as_ref(),
                                        );
                                        notify_measure.stop();
                                        *total_notify_us += notify_measure.as_us();
                                    }
                                }
                            }
                        }

                        // Existing: notify signatures for RPC subscriptions
                        let transactions = Self::get_transaction_signatures(entries);
                        if !transactions.is_empty() {
                            rpc_subscriptions.notify_signatures_received((slot, transactions));
                        }
                    }
                    Err(e) => warn!("completed-data-set-service deserialize error: {e:?}"),
                }
                slot
            };

        let slots = completed_sets_receiver
            .recv_timeout(RECV_TIMEOUT)
            .map(std::iter::once)?
            .chain(completed_sets_receiver.try_iter())
            .flatten()
            .map(|info| {
                handle_completed_data_set_info(
                    info,
                    &mut total_lut_load_us,
                    &mut total_notify_us,
                    &mut total_transactions,
                    &mut total_entries,
                    &mut total_data_sets,
                    &mut lut_transactions,
                )
            });

        if let Some(slot) = slots.max() {
            max_slots.shred_insert.fetch_max(slot, Ordering::Relaxed);
        }

        batch_measure.stop();

        // Report metrics if we processed any transactions
        if total_transactions > 0 {
            datapoint_info!(
                "deshred_geyser_timing",
                ("batch_total_us", batch_measure.as_us() as i64, i64),
                ("notify_total_us", total_notify_us as i64, i64),
                ("lut_load_total_us", total_lut_load_us as i64, i64),
                ("transactions_count", total_transactions as i64, i64),
                ("lut_transactions_count", lut_transactions as i64, i64),
                ("entries_count", total_entries as i64, i64),
                ("data_sets_count", total_data_sets as i64, i64),
                (
                    "avg_notify_us",
                    (total_notify_us / total_transactions) as i64,
                    i64
                ),
            );
        }

        Ok(())
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
        super::*, solana_hash::Hash, solana_keypair::Keypair, solana_signer::Signer,
        solana_transaction::Transaction,
    };

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
}
