use {
    crate::entry_notifier_interface::{EntryNotifierArc, EntryUpdateParentInfo},
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender, TrySendError, bounded},
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
    UpdateParent(EntryUpdateParentInfo),
}

pub type EntryNotifierSender = Sender<EntryNotification>;
pub type EntryNotifierReceiver = Receiver<EntryNotification>;

/// Maximum number of entry notifications buffered while a notifier is busy.
/// Blocking producers on saturation preserves notifications without allowing an
/// arbitrarily slow notifier to grow validator memory without bound.
const ENTRY_NOTIFICATION_CHANNEL_CAPACITY: usize = 2048;

/// Try the nonblocking fast path first so saturation is observable, then block
/// rather than dropping an entry notification.
pub fn send_entry_notification(
    sender: &EntryNotifierSender,
    notification: EntryNotification,
) -> Result<(), SendError<EntryNotification>> {
    match sender.try_send(notification) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(notification)) => {
            log::error!(
                "EntryNotifier channel is full (capacity {ENTRY_NOTIFICATION_CHANNEL_CAPACITY}); \
                 blocking to preserve the notification"
            );
            sender.send(notification)
        }
        Err(TrySendError::Disconnected(notification)) => Err(SendError(notification)),
    }
}

pub struct EntryNotifierService {
    sender: EntryNotifierSender,
    thread_hdl: JoinHandle<()>,
}

impl EntryNotifierService {
    pub fn new(entry_notifier: EntryNotifierArc, exit: Arc<AtomicBool>) -> Self {
        let (entry_notification_sender, entry_notification_receiver) =
            bounded(ENTRY_NOTIFICATION_CHANNEL_CAPACITY);
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
            EntryNotification::UpdateParent(update_parent) => {
                entry_notifier.notify_entry_update_parent(&update_parent)
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
        UpdateParent(Slot, BankId, Slot, Hash),
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

        fn notify_entry_update_parent(&self, update_parent: &EntryUpdateParentInfo) {
            self.events.lock().unwrap().push(TestEvent::UpdateParent(
                update_parent.slot,
                update_parent.cleared_bank_id,
                update_parent.parent_slot,
                update_parent.parent_block_id,
            ));
        }
    }

    #[test]
    fn test_forwards_entry_notifications_in_order() {
        let (sender, receiver) = bounded(3);
        let notifier = Arc::new(TestEntryNotifier::default());
        let block_footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 123,
            block_user_agent: b"test-validator".to_vec(),
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });
        let parent_block_id = Hash::new_unique();

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
            .send(EntryNotification::UpdateParent(EntryUpdateParentInfo {
                slot: 42,
                cleared_bank_id: 9,
                parent_slot: 40,
                parent_block_id,
            }))
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
                TestEvent::UpdateParent(42, 9, 40, parent_block_id),
                TestEvent::BlockFooter {
                    slot: 42,
                    bank_id: 9,
                    block_footer: Box::new(block_footer),
                },
            ]
        );
    }
}
