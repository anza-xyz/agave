use {
    crate::slot_status_notifier::SlotStatusNotifier,
    crossbeam_channel::RecvTimeoutError,
    solana_ledger::blockstore::CompletedSlotsReceiver,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::Duration,
    },
};

pub const COMPLETE_SLOT_REPORT_SLEEP_MS: u64 = 100;

pub struct CompletedSlotsService;
impl CompletedSlotsService {
    pub fn spawn(
        completed_slots_receiver: CompletedSlotsReceiver,
        slot_status_notifier: SlotStatusNotifier,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solComplSlot".to_string())
            .spawn(move || {
                loop {
                    // received exit signal, shutdown the service
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    match completed_slots_receiver
                        .recv_timeout(Duration::from_millis(COMPLETE_SLOT_REPORT_SLEEP_MS))
                    {
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            info!("CompletedSlotsService channel disconnected, exiting.");
                            break;
                        }
                        Ok(slots) => {
                            for slot in slots {
                                slot_status_notifier.read().unwrap().notify_completed(slot);
                            }
                        }
                    }
                }
            })
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::slot_status_notifier::SlotStatusNotifierInterface,
        crossbeam_channel::{Sender, unbounded},
        solana_clock::Slot,
        solana_runtime::bank::BankId,
        std::sync::RwLock,
    };

    struct CompletedSlotNotifier(Sender<Slot>);

    impl SlotStatusNotifierInterface for CompletedSlotNotifier {
        fn notify_completed(&self, slot: Slot) {
            self.0.send(slot).unwrap();
        }
        fn notify_slot_confirmed(&self, _: Slot, _: Option<Slot>, _: BankId) {
            unreachable!();
        }
        fn notify_slot_processed(&self, _: Slot, _: Option<Slot>, _: BankId) {
            unreachable!();
        }
        fn notify_slot_rooted(&self, _: Slot, _: Option<Slot>, _: BankId) {
            unreachable!();
        }
        fn notify_first_shred_received(&self, _: Slot) {
            unreachable!();
        }
        fn notify_created_bank(&self, _: Slot, _: Slot, _: BankId) {
            unreachable!();
        }
        fn notify_dead_bank(&self, _: Slot, _: Slot, _: BankId, _: String) {
            unreachable!();
        }
    }

    #[test]
    fn completed_slots_reach_geyser_without_rpc() {
        let (sender, receiver) = unbounded();
        let (notifications, received) = unbounded();
        let exit = Arc::new(AtomicBool::new(false));
        let service = CompletedSlotsService::spawn(
            receiver,
            Arc::new(RwLock::new(CompletedSlotNotifier(notifications))),
            exit,
        );
        sender.send(vec![4, 5]).unwrap();
        for slot in [4, 5] {
            assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), slot);
        }
        drop(sender);
        service.join().unwrap();
    }
}
