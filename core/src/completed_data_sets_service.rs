//! [`CompletedDataSetsService`] is a hub, that runs different operations when a "completed data
//! set", also known as a [`Vec<Entry>`], is received by the validator.
//!
//! Currently, `WindowService` sends [`CompletedDataSetInfo`]s via a `completed_sets_receiver`
//! provided to the [`CompletedDataSetsService`].

use {
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender},
    solana_entry::entry::Entry,
    solana_ledger::blockstore::{Blockstore, CompletedDataSetInfo},
    solana_rpc::{max_slots::MaxSlots, rpc_subscriptions::RpcSubscriptions},
    solana_sdk::signature::Signature,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub type CompletedDataSetsReceiver = Receiver<Vec<CompletedDataSetInfo>>;
pub type CompletedDataSetsSender = Sender<Vec<CompletedDataSetInfo>>;

pub struct CompletedDataSetsService {
    thread_hdl: JoinHandle<()>,
}

impl CompletedDataSetsService {
    pub fn new(
        completed_sets_receiver: CompletedDataSetsReceiver,
        blockstore: Arc<Blockstore>,
        rpc_subscriptions: Arc<RpcSubscriptions>,
        exit: Arc<AtomicBool>,
        max_slots: Arc<MaxSlots>,
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
                        &max_slots,
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
        max_slots: &Arc<MaxSlots>,
    ) -> Result<(), RecvTimeoutError> {
        const RECV_TIMEOUT: Duration = Duration::from_secs(1);
        let handle_completed_data_set_info = |completed_data_set_info| {
            let CompletedDataSetInfo { slot, indices } = completed_data_set_info;
            match blockstore.get_entries_in_data_block(slot, indices, /*slot_meta:*/ None) {
                Ok(entries) => {
                    let transactions = Self::get_transaction_signatures(entries);
                    if !transactions.is_empty() {
                        rpc_subscriptions.notify_signatures_received((slot, transactions));
                    }
                }
                Err(e) => warn!("completed-data-set-service deserialize error: {:?}", e),
            }
            slot
        };
        let slots = completed_sets_receiver
            .recv_timeout(RECV_TIMEOUT)
            .map(std::iter::once)?
            .chain(completed_sets_receiver.try_iter())
            .flatten()
            .map(handle_completed_data_set_info);
        if let Some(slot) = slots.max() {
            max_slots.shred_insert.fetch_max(slot, Ordering::Relaxed);
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
        super::*,
        solana_sdk::{
            hash::Hash,
            signature::{Keypair, Signer},
            transaction::Transaction,
        },
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
