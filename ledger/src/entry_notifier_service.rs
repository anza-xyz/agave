use {
    crate::entry_notifier_interface::EntryNotifierArc,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded},
    solana_clock::{BankId, Slot},
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

pub struct EntryNotification {
    pub slot: Slot,
    pub bank_id: BankId,
    pub index: usize,
    pub entry: EntrySummary,
    pub starting_transaction_index: usize,
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
        let EntryNotification {
            slot,
            bank_id,
            index,
            entry,
            starting_transaction_index,
        } = entry_notification_receiver.recv_timeout(Duration::from_secs(1))?;
        entry_notifier.notify_entry(slot, bank_id, index, &entry, starting_transaction_index);
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

    type NotifiedEntry = (Slot, BankId, usize, usize);

    #[derive(Debug, Default)]
    struct TestEntryNotifier {
        notified_entries: Mutex<Vec<NotifiedEntry>>,
    }

    impl EntryNotifier for TestEntryNotifier {
        fn notify_entry(
            &self,
            slot: Slot,
            bank_id: BankId,
            index: usize,
            _entry: &EntrySummary,
            starting_transaction_index: usize,
        ) {
            self.notified_entries.lock().unwrap().push((
                slot,
                bank_id,
                index,
                starting_transaction_index,
            ));
        }
    }

    #[test]
    fn test_notify_entry_forwards_bank_id() {
        let entry_notifier = Arc::new(TestEntryNotifier::default());
        let (entry_notification_sender, entry_notification_receiver) = unbounded();
        entry_notification_sender
            .send(EntryNotification {
                slot: 42,
                bank_id: 9,
                index: 3,
                entry: EntrySummary {
                    num_hashes: 1,
                    hash: Hash::new_unique(),
                    num_transactions: 2,
                },
                starting_transaction_index: 7,
            })
            .unwrap();

        EntryNotifierService::notify_entry(&entry_notification_receiver, entry_notifier.clone())
            .unwrap();

        assert_eq!(
            *entry_notifier.notified_entries.lock().unwrap(),
            vec![(42, 9, 3, 7)]
        );
    }
}
