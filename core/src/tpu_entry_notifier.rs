use {
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender},
    solana_entry::{
        block_component::{VersionedBlockMarker, VersionedUpdateParent},
        entry::EntrySummary,
        entry_or_marker::EntryOrMarker,
    },
    solana_ledger::{
        blockstore::UpdateParentSignal,
        entry_notifier_service::{EntryNotification, EntryNotifierSender},
    },
    solana_poh::poh_recorder::WorkingBankEntryOrMarker,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub(crate) struct TpuEntryNotifier {
    thread_hdl: JoinHandle<()>,
}

impl TpuEntryNotifier {
    pub(crate) fn new(
        entry_receiver: Receiver<WorkingBankEntryOrMarker>,
        entry_notification_sender: EntryNotifierSender,
        broadcast_entry_sender: Sender<WorkingBankEntryOrMarker>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solTpuEntry".to_string())
            .spawn(move || {
                let mut current_slot = 0;
                let mut current_index = 0;
                let mut current_transaction_index = 0;
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    if let Err(RecvTimeoutError::Disconnected) = Self::send_entry_notification(
                        exit.clone(),
                        &entry_receiver,
                        &entry_notification_sender,
                        &broadcast_entry_sender,
                        &mut current_slot,
                        &mut current_index,
                        &mut current_transaction_index,
                    ) {
                        break;
                    }
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    pub(crate) fn send_entry_notification(
        exit: Arc<AtomicBool>,
        entry_receiver: &Receiver<WorkingBankEntryOrMarker>,
        entry_notification_sender: &EntryNotifierSender,
        broadcast_entry_sender: &Sender<WorkingBankEntryOrMarker>,
        current_slot: &mut u64,
        current_index: &mut usize,
        current_transaction_index: &mut usize,
    ) -> Result<(), RecvTimeoutError> {
        let (bank, (entry_or_marker, tick_height)) =
            entry_receiver.recv_timeout(Duration::from_secs(1))?;
        let slot = bank.slot();
        let index = if slot != *current_slot {
            *current_index = 0;
            *current_transaction_index = 0;
            *current_slot = slot;
            0
        } else {
            *current_index += 1;
            *current_index
        };

        match &entry_or_marker {
            EntryOrMarker::Entry(entry) => {
                let entry_summary = EntrySummary {
                    num_hashes: entry.num_hashes,
                    hash: entry.hash,
                    num_transactions: entry.transactions.len() as u64,
                };
                if let Err(err) = entry_notification_sender.send(EntryNotification::Entry {
                    slot,
                    index,
                    entry: entry_summary,
                    starting_transaction_index: *current_transaction_index,
                }) {
                    warn!(
                        "Failed to send slot {slot:?} entry {index:?} from Tpu to \
                         EntryNotifierService, error {err:?}",
                    );
                }
                *current_transaction_index += entry.transactions.len();
            }
            EntryOrMarker::Marker(marker) => {
                if let Some(update_parent) = update_parent_signal_from_marker(slot, index, marker) {
                    if let Err(err) = entry_notification_sender
                        .send(EntryNotification::UpdateParent(update_parent))
                    {
                        warn!(
                            "Failed to send slot {slot:?} UpdateParent {index:?} from Tpu to \
                             EntryNotifierService, error {err:?}",
                        );
                    }
                    *current_slot = u64::MAX;
                    *current_index = 0;
                    *current_transaction_index = 0;
                }
            }
        }

        if let Err(err) = broadcast_entry_sender.send((bank, (entry_or_marker, tick_height))) {
            let index = *current_index;
            warn!(
                "Failed to send slot {slot:?} entry/marker {index:?} from Tpu to BroadcastStage, \
                 error {err:?}",
            );
            // If the BroadcastStage channel is closed, the validator has halted. Try to exit
            // gracefully.
            exit.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

fn update_parent_signal_from_marker(
    slot: u64,
    marker_index: usize,
    marker: &VersionedBlockMarker,
) -> Option<UpdateParentSignal> {
    let update_parent = match marker {
        VersionedBlockMarker::V1(marker) => marker.as_update_parent()?,
    };
    let update_parent_fec_set_index = u32::try_from(marker_index).unwrap_or_else(|_| {
        warn!(
            "TPU UpdateParent marker index {marker_index} for slot {slot} exceeded u32::MAX; \
             saturating invalidation boundary"
        );
        u32::MAX
    });
    match update_parent {
        VersionedUpdateParent::V1(update_parent) => Some(UpdateParentSignal {
            slot,
            update_parent_fec_set_index,
            parent_slot: update_parent.new_parent_slot,
            parent_block_id: Some(update_parent.new_parent_block_id),
        }),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::unbounded,
        solana_entry::{block_component::UpdateParentV1, entry::Entry},
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_runtime::bank::Bank,
    };

    #[test]
    fn test_tpu_entry_notifier_sends_update_parent_and_resets_indexes() {
        let bank = Arc::new(Bank::new_for_tests(&GenesisConfig::default()));
        let slot = bank.slot();
        let parent_block_id = Hash::new_unique();
        let (entry_sender, entry_receiver) = unbounded();
        let (entry_notification_sender, entry_notification_receiver) = unbounded();
        let (broadcast_entry_sender, _broadcast_entry_receiver) = unbounded();
        let exit = Arc::new(AtomicBool::new(false));
        let mut current_slot = u64::MAX;
        let mut current_index = 0;
        let mut current_transaction_index = 0;

        entry_sender
            .send((
                bank.clone(),
                (
                    EntryOrMarker::Entry(Entry::new(&Hash::default(), 1, vec![])),
                    0,
                ),
            ))
            .unwrap();
        entry_sender
            .send((
                bank.clone(),
                (
                    EntryOrMarker::Marker(VersionedBlockMarker::from_update_parent(
                        UpdateParentV1 {
                            new_parent_slot: 0,
                            new_parent_block_id: parent_block_id,
                        },
                    )),
                    0,
                ),
            ))
            .unwrap();
        entry_sender
            .send((
                bank,
                (
                    EntryOrMarker::Entry(Entry::new(&Hash::default(), 1, vec![])),
                    0,
                ),
            ))
            .unwrap();

        for _ in 0..3 {
            TpuEntryNotifier::send_entry_notification(
                exit.clone(),
                &entry_receiver,
                &entry_notification_sender,
                &broadcast_entry_sender,
                &mut current_slot,
                &mut current_index,
                &mut current_transaction_index,
            )
            .unwrap();
        }

        match entry_notification_receiver.try_recv().unwrap() {
            EntryNotification::Entry {
                slot: entry_slot,
                index,
                starting_transaction_index,
                ..
            } => {
                assert_eq!(entry_slot, slot);
                assert_eq!(index, 0);
                assert_eq!(starting_transaction_index, 0);
            }
            EntryNotification::UpdateParent(_) => panic!("expected entry notification"),
        }
        match entry_notification_receiver.try_recv().unwrap() {
            EntryNotification::UpdateParent(update_parent) => {
                assert_eq!(
                    update_parent,
                    UpdateParentSignal {
                        slot,
                        update_parent_fec_set_index: 1,
                        parent_slot: 0,
                        parent_block_id: Some(parent_block_id),
                    }
                );
            }
            EntryNotification::Entry { .. } => panic!("expected UpdateParent notification"),
        }
        match entry_notification_receiver.try_recv().unwrap() {
            EntryNotification::Entry {
                slot: entry_slot,
                index,
                starting_transaction_index,
                ..
            } => {
                assert_eq!(entry_slot, slot);
                assert_eq!(index, 0);
                assert_eq!(starting_transaction_index, 0);
            }
            EntryNotification::UpdateParent(_) => panic!("expected replacement entry notification"),
        }
        assert!(entry_notification_receiver.try_recv().is_err());
    }
}
