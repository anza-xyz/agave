use {
    crate::entry_notifier_interface::EntryNotifierArc,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded},
    solana_clock::{BankId, Slot},
    solana_entry::{block_component::VersionedBlockFooter, entry::EntrySummary},
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
        bank_id: BankId,
        index: usize,
        entry: EntrySummary,
        starting_transaction_index: usize,
    },
    BlockFooter {
        slot: Slot,
        bank_id: BankId,
        block_footer: Box<VersionedBlockFooter>,
    },
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
                        Self::notify(&entry_notification_receiver, entry_notifier.clone())
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

    fn notify(
        entry_notification_receiver: &EntryNotifierReceiver,
        entry_notifier: EntryNotifierArc,
    ) -> Result<(), RecvTimeoutError> {
        match entry_notification_receiver.recv_timeout(Duration::from_secs(1))? {
            EntryNotification::Entry {
                slot,
                bank_id,
                index,
                entry,
                starting_transaction_index,
            } => {
                entry_notifier.notify_entry(
                    slot,
                    bank_id,
                    index,
                    &entry,
                    starting_transaction_index,
                );
            }
            EntryNotification::BlockFooter {
                slot,
                bank_id,
                block_footer,
            } => entry_notifier.notify_block_footer(slot, bank_id, block_footer.as_ref()),
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
        super::*, crate::entry_notifier_interface::EntryNotifier,
        solana_entry::block_component::BlockFooterV1, solana_hash::Hash, std::sync::Mutex,
    };

    #[derive(Debug, PartialEq, Eq)]
    enum TestEvent {
        Entry {
            slot: Slot,
            bank_id: BankId,
            index: usize,
            starting_transaction_index: usize,
        },
        BlockFooter {
            slot: Slot,
            bank_id: BankId,
            block_footer: Box<VersionedBlockFooter>,
        },
    }

    #[derive(Default)]
    struct TestEntryNotifier {
        events: Mutex<Vec<TestEvent>>,
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
            self.events.lock().unwrap().push(TestEvent::Entry {
                slot,
                bank_id,
                index,
                starting_transaction_index,
            });
        }

        fn notify_block_footer(
            &self,
            slot: Slot,
            bank_id: BankId,
            block_footer: &VersionedBlockFooter,
        ) {
            self.events.lock().unwrap().push(TestEvent::BlockFooter {
                slot,
                bank_id,
                block_footer: Box::new(block_footer.clone()),
            });
        }
    }

    #[test]
    fn test_forwards_entry_and_block_footer_in_order() {
        let (sender, receiver) = unbounded();
        let notifier = Arc::new(TestEntryNotifier::default());
        let block_footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 123,
            block_user_agent: b"test-validator".to_vec(),
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });

        sender
            .send(EntryNotification::Entry {
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
        sender
            .send(EntryNotification::BlockFooter {
                slot: 42,
                bank_id: 9,
                block_footer: Box::new(block_footer.clone()),
            })
            .unwrap();

        EntryNotifierService::notify(&receiver, notifier.clone()).unwrap();
        EntryNotifierService::notify(&receiver, notifier.clone()).unwrap();

        assert_eq!(
            *notifier.events.lock().unwrap(),
            vec![
                TestEvent::Entry {
                    slot: 42,
                    bank_id: 9,
                    index: 3,
                    starting_transaction_index: 7,
                },
                TestEvent::BlockFooter {
                    slot: 42,
                    bank_id: 9,
                    block_footer: Box::new(block_footer),
                },
            ]
        );
    }
}
