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
        crossbeam_channel::{Sender, unbounded},
        solana_clock::Slot,
        solana_rpc::slot_status_notifier::SlotStatusNotifierInterface,
        solana_runtime::bank::BankId,
        std::sync::RwLock,
    };

    #[derive(Debug, PartialEq)]
    enum Status {
        Confirmed(Slot, Option<Slot>, BankId),
        Processed(Slot, Option<Slot>, BankId),
        Rooted(Slot, Option<Slot>, BankId),
    }

    struct RecordingNotifier(Sender<Status>);

    impl SlotStatusNotifierInterface for RecordingNotifier {
        fn notify_slot_confirmed(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
            self.0
                .send(Status::Confirmed(slot, parent, bank_id))
                .unwrap();
        }
        fn notify_slot_processed(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
            self.0
                .send(Status::Processed(slot, parent, bank_id))
                .unwrap();
        }
        fn notify_slot_rooted(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
            self.0.send(Status::Rooted(slot, parent, bank_id)).unwrap();
        }
        fn notify_first_shred_received(&self, _: Slot) {}
        fn notify_completed(&self, _: Slot) {}
        fn notify_created_bank(&self, _: Slot, _: Slot, _: BankId) {}
        fn notify_slot_dead(&self, _: Slot, _: Slot, _: String) {}
    }

    fn recording_notifier() -> (SlotStatusNotifier, Receiver<Status>) {
        let (sender, receiver) = unbounded();
        (Arc::new(RwLock::new(RecordingNotifier(sender))), receiver)
    }

    #[test]
    fn test_slot_status_observer_joins_on_idle_channel() {
        let (sender, receiver) = unbounded();
        let (notifier, statuses) = recording_notifier();
        let observer = SlotStatusObserver::new(receiver, notifier);

        let bank_id = BankId::new(3);
        sender
            .send(SlotNotification::OptimisticallyConfirmed(7, bank_id))
            .unwrap();
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(5)).unwrap(),
            Status::Confirmed(7, None, bank_id),
        );

        let (done_tx, done_rx) = unbounded();
        let joiner = thread::spawn(move || {
            let mut observer = observer;
            observer.join().unwrap();
            done_tx.send(()).unwrap();
        });
        let result = done_rx.recv_timeout(Duration::from_secs(5));
        // Release a blocked receiver even if the shutdown regression returns.
        drop(sender);
        joiner.join().unwrap();
        result.expect("observer join timed out on an idle channel");
    }

    #[test]
    fn test_slot_status_observer_exits_on_disconnect() {
        let (sender, receiver) = unbounded();
        let (notifier, _statuses) = recording_notifier();
        let mut observer = SlotStatusObserver::new(receiver, notifier);

        drop(sender);
        let handle = observer.bank_notification_receiver_service.take().unwrap();
        for _ in 0..100 {
            if handle.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let finished = handle.is_finished();
        assert!(!observer.exit_updated_slot_server.load(Ordering::Relaxed));
        // Also stop the worker when testing a regressed disconnect path.
        observer
            .exit_updated_slot_server
            .store(true, Ordering::Relaxed);
        handle.join().unwrap();
        assert!(finished);
    }

    #[test]
    fn test_slot_status_observer_delivers_after_idle_timeout() {
        let (sender, receiver) = unbounded();
        let (notifier, statuses) = recording_notifier();
        let mut observer = SlotStatusObserver::new(receiver, notifier);
        let bank_id = BankId::new(9);
        sender
            .send(SlotNotification::OptimisticallyConfirmed(42, bank_id))
            .unwrap();
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(5)).unwrap(),
            Status::Confirmed(42, None, bank_id),
        );
        // Leave the input connected and idle longer than the receive timeout.
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(1_200)),
            Err(RecvTimeoutError::Timeout),
        );
        sender
            .send(SlotNotification::Frozen((43, 42, bank_id)))
            .unwrap();
        sender
            .send(SlotNotification::Root((43, 42, bank_id)))
            .unwrap();
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(5)).unwrap(),
            Status::Processed(43, Some(42), bank_id),
        );
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(5)).unwrap(),
            Status::Rooted(43, Some(42), bank_id),
        );
        drop(sender);
        observer.join().unwrap();
    }

    #[test]
    fn test_slot_status_observer_drains_queued_notifications_on_disconnect() {
        let (sender, receiver) = unbounded();
        let (notifier, statuses) = recording_notifier();
        let bank_id = BankId::new(11);
        sender
            .send(SlotNotification::Frozen((44, 42, bank_id)))
            .unwrap();
        sender
            .send(SlotNotification::OptimisticallyConfirmed(44, bank_id))
            .unwrap();
        sender
            .send(SlotNotification::Root((44, 42, bank_id)))
            .unwrap();
        drop(sender);
        // Start with a disconnected channel that still contains notifications.
        let mut observer = SlotStatusObserver::new(receiver, notifier);
        for expected in [
            Status::Processed(44, Some(42), bank_id),
            Status::Confirmed(44, None, bank_id),
            Status::Rooted(44, Some(42), bank_id),
        ] {
            assert_eq!(
                statuses.recv_timeout(Duration::from_secs(5)).unwrap(),
                expected
            );
        }
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected),
        );
        assert!(!observer.exit_updated_slot_server.load(Ordering::Relaxed));
        observer.join().unwrap();
    }
}
