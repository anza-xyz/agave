use {
    crossbeam_channel::{Receiver, RecvTimeoutError},
    solana_rpc::{
        optimistically_confirmed_bank_tracker::SlotNotification,
        slot_status_notifier::SlotStatusNotifier,
    },
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

#[derive(Debug)]
pub(crate) struct SlotStatusObserver {
    bank_notification_receiver_service: Option<JoinHandle<()>>,
    exit_updated_slot_server: Arc<AtomicBool>,
}

impl SlotStatusObserver {
    pub fn new(
        bank_notification_receiver: Receiver<SlotNotification>,
        slot_status_notifier: SlotStatusNotifier,
    ) -> Self {
        let exit_updated_slot_server = Arc::new(AtomicBool::new(false));

        Self {
            bank_notification_receiver_service: Some(Self::run_bank_notification_receiver(
                bank_notification_receiver,
                exit_updated_slot_server.clone(),
                slot_status_notifier,
            )),
            exit_updated_slot_server,
        }
    }

    pub fn join(&mut self) -> thread::Result<()> {
        self.exit_updated_slot_server.store(true, Ordering::Relaxed);
        self.bank_notification_receiver_service
            .take()
            .map(JoinHandle::join)
            .unwrap()
    }

    fn run_bank_notification_receiver(
        bank_notification_receiver: Receiver<SlotNotification>,
        exit: Arc<AtomicBool>,
        slot_status_notifier: SlotStatusNotifier,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solBankNotif".to_string())
            .spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    // A blocking recv would keep the exit flag unobserved until the
                    // next notification arrives, so shutdown would hang on an idle
                    // cluster waiting for the thread to join.
                    match bank_notification_receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(slot) => match slot {
                            SlotNotification::OptimisticallyConfirmed(slot, bank_id) => {
                                slot_status_notifier
                                    .read()
                                    .unwrap()
                                    .notify_slot_confirmed(slot, None, bank_id);
                            }
                            SlotNotification::Frozen((slot, parent, bank_id)) => {
                                slot_status_notifier.read().unwrap().notify_slot_processed(
                                    slot,
                                    Some(parent),
                                    bank_id,
                                );
                            }
                            SlotNotification::Root((slot, parent, bank_id)) => {
                                slot_status_notifier.read().unwrap().notify_slot_rooted(
                                    slot,
                                    Some(parent),
                                    bank_id,
                                );
                            }
                        },
                        // No more notifications will ever arrive on a disconnected
                        // channel; spinning would just burn the core.
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
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
        solana_clock::Slot,
        solana_rpc::{
            optimistically_confirmed_bank_tracker::SlotNotification,
            slot_status_notifier::SlotStatusNotifierInterface,
        },
        solana_runtime::bank::BankId,
        std::sync::RwLock,
    };

    #[derive(Debug)]
    struct NoopNotifier;

    impl SlotStatusNotifierInterface for NoopNotifier {
        fn notify_slot_confirmed(&self, _: Slot, _: Option<Slot>, _: BankId) {}
        fn notify_slot_processed(&self, _: Slot, _: Option<Slot>, _: BankId) {}
        fn notify_slot_rooted(&self, _: Slot, _: Option<Slot>, _: BankId) {}
        fn notify_first_shred_received(&self, _: Slot) {}
        fn notify_completed(&self, _: Slot) {}
        fn notify_created_bank(&self, _: Slot, _: Slot, _: BankId) {}
        fn notify_slot_dead(&self, _: Slot, _: Slot, _: String) {}
    }

    #[derive(Debug)]
    struct SignalNotifier(Arc<AtomicBool>);

    impl SlotStatusNotifierInterface for SignalNotifier {
        fn notify_slot_confirmed(&self, _: Slot, _: Option<Slot>, _: BankId) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn notify_slot_processed(&self, _: Slot, _: Option<Slot>, _: BankId) {}
        fn notify_slot_rooted(&self, _: Slot, _: Option<Slot>, _: BankId) {}
        fn notify_first_shred_received(&self, _: Slot) {}
        fn notify_completed(&self, _: Slot) {}
        fn notify_created_bank(&self, _: Slot, _: Slot, _: BankId) {}
        fn notify_slot_dead(&self, _: Slot, _: Slot, _: String) {}
    }

    fn wait_until_entered(entered: &AtomicBool) {
        for _ in 0..100 {
            if entered.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("worker never entered the receive path");
    }

    #[test]
    fn test_slot_status_observer_joins_on_idle_channel() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let entered = Arc::new(AtomicBool::new(false));
        let notifier: SlotStatusNotifier = Arc::new(RwLock::new(SignalNotifier(entered.clone())));
        let observer = SlotStatusObserver::new(receiver, notifier);

        sender
            .send(SlotNotification::OptimisticallyConfirmed(
                0,
                BankId::default(),
            ))
            .unwrap();
        wait_until_entered(&entered);

        let (done_tx, done_rx) = crossbeam_channel::unbounded();
        thread::spawn(move || {
            let mut observer = observer;
            observer.join().unwrap();
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("observer join timed out on an idle channel");
        drop(sender);
    }

    #[test]
    fn test_slot_status_observer_exits_on_disconnect() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let notifier: SlotStatusNotifier = Arc::new(RwLock::new(NoopNotifier));
        let mut observer = SlotStatusObserver::new(receiver, notifier);

        drop(sender);
        let handle = observer.bank_notification_receiver_service.take().unwrap();
        for _ in 0..100 {
            if handle.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished());
        assert!(!observer.exit_updated_slot_server.load(Ordering::Relaxed));
        handle.join().unwrap();
    }
}
