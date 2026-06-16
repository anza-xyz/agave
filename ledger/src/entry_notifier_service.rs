use {
    crate::{blockstore::UpdateParentSignal, entry_notifier_interface::EntryNotifierArc},
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded},
    solana_clock::Slot,
    solana_entry::entry::EntrySummary,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub enum EntryNotification {
    Entry {
        slot: Slot,
        index: usize,
        entry: EntrySummary,
        starting_transaction_index: usize,
    },
    /// Invalidate same-slot entries and transactions observed before an
    /// Alpenglow UpdateParent marker. Delivery is ordered with entry
    /// notifications on this channel and is best-effort during shutdown.
    UpdateParent(UpdateParentSignal),
}

pub type EntryNotifierSender = Sender<EntryNotification>;
pub type EntryNotifierReceiver = Receiver<EntryNotification>;

pub struct EntryNotifierService {
    sender: EntryNotifierSender,
    thread_hdl: JoinHandle<()>,
}

impl EntryNotifierService {
    pub fn new(entry_notifier: EntryNotifierArc, exit: Arc<AtomicBool>) -> Self {
        let (entry_notification_sender, entry_notification_receiver) = unbounded();
        let thread_hdl = Builder::new()
            .name("solEntryNotif".to_string())
            .spawn(move || {
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    if let Err(RecvTimeoutError::Disconnected) =
                        Self::notify_entry(&entry_notification_receiver, entry_notifier.clone())
                    {
                        break;
                    }
                }
            })
            .unwrap();
        Self {
            sender: entry_notification_sender,
            thread_hdl,
        }
    }

    fn notify_entry(
        entry_notification_receiver: &EntryNotifierReceiver,
        entry_notifier: EntryNotifierArc,
    ) -> Result<(), RecvTimeoutError> {
        match entry_notification_receiver.recv_timeout(Duration::from_secs(1))? {
            EntryNotification::Entry {
                slot,
                index,
                entry,
                starting_transaction_index,
            } => entry_notifier.notify_entry(slot, index, &entry, starting_transaction_index),
            EntryNotification::UpdateParent(update_parent) => {
                entry_notifier.notify_update_parent(&update_parent)
            }
        }
        Ok(())
    }

    pub fn sender(&self) -> &EntryNotifierSender {
        &self.sender
    }

    pub fn sender_cloned(&self) -> EntryNotifierSender {
        self.sender.clone()
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::entry_notifier_interface::EntryNotifier, solana_hash::Hash,
        std::sync::Mutex,
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum TestEvent {
        Entry {
            slot: Slot,
            index: usize,
            num_hashes: u64,
            hash: Hash,
            num_transactions: u64,
            starting_transaction_index: usize,
        },
        UpdateParent(UpdateParentSignal),
    }

    #[derive(Default)]
    struct TestEntryNotifier {
        events: Mutex<Vec<TestEvent>>,
    }

    impl EntryNotifier for TestEntryNotifier {
        fn notify_entry(
            &self,
            slot: Slot,
            index: usize,
            entry: &EntrySummary,
            starting_transaction_index: usize,
        ) {
            self.events.lock().unwrap().push(TestEvent::Entry {
                slot,
                index,
                num_hashes: entry.num_hashes,
                hash: entry.hash,
                num_transactions: entry.num_transactions,
                starting_transaction_index,
            });
        }

        fn notify_update_parent(&self, update_parent: &UpdateParentSignal) {
            self.events
                .lock()
                .unwrap()
                .push(TestEvent::UpdateParent(update_parent.clone()));
        }
    }

    #[test]
    fn test_entry_notifier_service_forwards_update_parent_in_order() {
        let (sender, receiver) = unbounded();
        let notifier = Arc::new(TestEntryNotifier::default());
        let entry_hash = Hash::new_unique();
        let entry_summary = || EntrySummary {
            num_hashes: 1,
            hash: entry_hash,
            num_transactions: 2,
        };
        let update_parent = UpdateParentSignal {
            slot: 7,
            update_parent_fec_set_index: 32,
            parent_slot: 3,
            parent_block_id: Some(Hash::new_unique()),
        };

        sender
            .send(EntryNotification::Entry {
                slot: 7,
                index: 0,
                entry: entry_summary(),
                starting_transaction_index: 0,
            })
            .unwrap();
        sender
            .send(EntryNotification::UpdateParent(update_parent.clone()))
            .unwrap();
        sender
            .send(EntryNotification::Entry {
                slot: 7,
                index: 0,
                entry: entry_summary(),
                starting_transaction_index: 0,
            })
            .unwrap();

        EntryNotifierService::notify_entry(&receiver, notifier.clone()).unwrap();
        EntryNotifierService::notify_entry(&receiver, notifier.clone()).unwrap();
        EntryNotifierService::notify_entry(&receiver, notifier.clone()).unwrap();

        assert_eq!(
            *notifier.events.lock().unwrap(),
            vec![
                TestEvent::Entry {
                    slot: 7,
                    index: 0,
                    num_hashes: 1,
                    hash: entry_hash,
                    num_transactions: 2,
                    starting_transaction_index: 0,
                },
                TestEvent::UpdateParent(update_parent),
                TestEvent::Entry {
                    slot: 7,
                    index: 0,
                    num_hashes: 1,
                    hash: entry_hash,
                    num_transactions: 2,
                    starting_transaction_index: 0,
                },
            ]
        );
    }
}
