//! Dispatch local leader observations away from banking and bank lifecycle locks.
use {
    agave_geyser_plugin_host::GeyserPluginHost,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        LeaderBankStatus as PluginBankStatus, ReplicaLeaderTransactionInfo,
        ReplicaLeaderTransactionInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_runtime_transaction::leader_transaction_notifier::{
        LeaderBankStatus, LeaderNotification, LeaderTransactionNotifier,
    },
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

// BankingStage normally targets 64 transactions per batch. Keep buffering bounded
// while leaving headroom for lifecycle events. Slow consumers must detect gaps.
const QUEUE_CAPACITY: usize = 1024;

pub(crate) struct LeaderNotificationService {
    pub notifier: LeaderTransactionNotifier,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LeaderNotificationService {
    pub fn new(manager: Arc<ArcSwap<GeyserPluginHost>>) -> Self {
        Self::with_capacity(manager, QUEUE_CAPACITY)
    }

    fn with_capacity(manager: Arc<ArcSwap<GeyserPluginHost>>, capacity: usize) -> Self {
        let (notifier, receiver) = LeaderTransactionNotifier::channel(capacity);
        let counters = notifier.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("solGeyserLeader".into())
            .spawn(move || {
                let mut reported_loss = (0, 0);
                let mut observed_loss = (0, 0);
                loop {
                    let message = if thread_stop.load(Ordering::Relaxed) {
                        receiver.try_recv().ok()
                    } else {
                        // `counters` holds a sender, so this can only time out.
                        receiver.recv_timeout(Duration::from_millis(100)).ok()
                    };
                    let host = manager.load();
                    let loss = counters.dropped();
                    if loss != observed_loss {
                        solana_metrics::datapoint_info!(
                            "geyser-leader-notifications",
                            ("dropped_transactions", loss.0 as i64, i64),
                            ("dropped_bank_statuses", loss.1 as i64, i64)
                        );
                        observed_loss = loss;
                    }
                    if loss != reported_loss && report_loss(&host, loss) {
                        reported_loss = loss;
                    }
                    match message {
                        Some(message) => dispatch(&host, &message),
                        None if thread_stop.load(Ordering::Relaxed) => break,
                        None => {}
                    }
                }
            })
            .expect("spawn leader notification dispatcher");
        Self {
            notifier,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for LeaderNotificationService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!("leader notification dispatcher panicked");
        }
    }
}

fn report_loss(host: &GeyserPluginHost, loss: (u64, u64)) -> bool {
    let mut success = true;
    for plugin in host
        .plugins()
        .filter(|p| p.leader_transaction_notifications_enabled())
    {
        if let Err(err) = plugin.notify_leader_notification_loss(loss.0, loss.1) {
            success = false;
            error!(
                "Leader notification loss callback failed for {}: {err}",
                plugin.name()
            );
        }
    }
    success
}

fn dispatch(host: &GeyserPluginHost, message: &LeaderNotification) {
    for plugin in host
        .plugins()
        .filter(|p| p.leader_transaction_notifications_enabled())
    {
        match message {
            LeaderNotification::Transactions(batch) => {
                for (offset, (transaction, status)) in batch.transactions().enumerate() {
                    let info = ReplicaLeaderTransactionInfo {
                        transaction,
                        status: status.as_ref().map(|_| ()),
                        index: batch
                            .starting_index
                            .checked_add(offset)
                            .expect("recorded transaction index fits usize"),
                    };
                    if let Err(err) = plugin.notify_leader_transaction(
                        ReplicaLeaderTransactionInfoVersions::V0_0_1(&info),
                        batch.slot,
                        batch.bank_id,
                    ) {
                        solana_metrics::inc_new_counter_error!(
                            "geyser-leader-transaction-callback-error",
                            1
                        );
                        error!(
                            "Leader transaction callback failed for {}: {err}",
                            plugin.name()
                        );
                    }
                }
            }
            LeaderNotification::Bank {
                slot,
                parent,
                bank_id,
                status,
            } => {
                let status = match *status {
                    LeaderBankStatus::Created => PluginBankStatus::Created,
                    LeaderBankStatus::Frozen { transaction_count } => {
                        PluginBankStatus::Frozen { transaction_count }
                    }
                    LeaderBankStatus::Abandoned { transaction_count } => {
                        PluginBankStatus::Abandoned { transaction_count }
                    }
                };
                if let Err(err) = plugin.notify_leader_bank_status(*slot, *parent, *bank_id, status)
                {
                    solana_metrics::inc_new_counter_error!("geyser-leader-bank-callback-error", 1);
                    error!("Leader bank callback failed for {}: {err}", plugin.name());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_geyser_plugin_host::LoadedGeyserPlugin,
        agave_geyser_plugin_interface::geyser_plugin_interface::{
            GeyserPlugin, GeyserPluginError, Result,
        },
        crossbeam_channel::{Receiver, Sender, bounded, unbounded},
        solana_runtime_transaction::{
            leader_transaction_notifier::LeaderTransactionBatch,
            runtime_transaction::RuntimeTransaction,
        },
        solana_transaction::Transaction,
        std::sync::{Mutex, atomic::AtomicUsize},
    };

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Transaction(u64, usize, bool),
        Bank(u64, PluginBankStatus),
        Loss(u64, u64),
    }

    #[derive(Debug)]
    struct Plugin {
        enabled: bool,
        fail: bool,
        events: Arc<Mutex<Vec<Event>>>,
    }
    impl GeyserPlugin for Plugin {
        fn name(&self) -> &'static str {
            "leader-test"
        }
        fn leader_transaction_notifications_enabled(&self) -> bool {
            self.enabled
        }
        fn notify_leader_transaction(
            &self,
            info: ReplicaLeaderTransactionInfoVersions,
            _slot: u64,
            bank: u64,
        ) -> Result<()> {
            let ReplicaLeaderTransactionInfoVersions::V0_0_1(info) = info;
            self.events.lock().unwrap().push(Event::Transaction(
                bank,
                info.index,
                info.status.is_ok(),
            ));
            if self.fail {
                Err(GeyserPluginError::Custom("test".into()))
            } else {
                Ok(())
            }
        }
        fn notify_leader_bank_status(
            &self,
            _slot: u64,
            _parent: u64,
            bank: u64,
            status: PluginBankStatus,
        ) -> Result<()> {
            self.events.lock().unwrap().push(Event::Bank(bank, status));
            if self.fail {
                Err(GeyserPluginError::Custom("test".into()))
            } else {
                Ok(())
            }
        }
        fn notify_leader_notification_loss(&self, transactions: u64, banks: u64) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(Event::Loss(transactions, banks));
            if self.fail {
                Err(GeyserPluginError::Custom("test".into()))
            } else {
                Ok(())
            }
        }
    }

    fn loaded(plugin: impl GeyserPlugin) -> Arc<LoadedGeyserPlugin> {
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this().into();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap().into();
        // This plugin's code is linked into the current test executable.
        Arc::new(unsafe { LoadedGeyserPlugin::new(library, Box::new(plugin), None) })
    }

    fn transactions() -> LeaderTransactionBatch {
        let tx = RuntimeTransaction::from_transaction_for_tests(Transaction::new_with_payer(
            &[],
            Some(&solana_pubkey::Pubkey::new_unique()),
        ));
        let mut batch = LeaderTransactionBatch::new(42, 7, 2, 256);
        batch.push(&tx, Ok(()));
        batch.push(
            &tx,
            Err(solana_transaction_error::TransactionError::AccountNotFound),
        );
        batch.starting_index = 3;
        batch
    }

    #[test]
    fn opt_in_errors_and_loss_reporting() {
        agave_logger::setup();
        let events: Vec<_> = (0..3).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
        let host = GeyserPluginHost::from_plugins(vec![
            loaded(Plugin {
                enabled: true,
                fail: true,
                events: events[0].clone(),
            }),
            loaded(Plugin {
                enabled: true,
                fail: false,
                events: events[1].clone(),
            }),
            loaded(Plugin {
                enabled: false,
                fail: false,
                events: events[2].clone(),
            }),
        ]);
        assert!(host.leader_transaction_notifications_enabled());
        dispatch(&host, &LeaderNotification::Transactions(transactions()));
        dispatch(
            &host,
            &LeaderNotification::Bank {
                slot: 42,
                parent: 41,
                bank_id: 7,
                status: LeaderBankStatus::Frozen {
                    transaction_count: 5,
                },
            },
        );
        assert!(!report_loss(&host, (3, 1))); // A callback failure requests a retry.
        assert_eq!(*events[0].lock().unwrap(), *events[1].lock().unwrap());
        assert_eq!(
            *events[1].lock().unwrap(),
            vec![
                Event::Transaction(7, 3, true),
                Event::Transaction(7, 4, false),
                Event::Bank(
                    7,
                    PluginBankStatus::Frozen {
                        transaction_count: 5
                    }
                ),
                Event::Loss(3, 1)
            ]
        );
        assert!(events[2].lock().unwrap().is_empty());
    }

    #[test]
    fn worker_retries_loss_reports_while_idle() {
        #[derive(Debug)]
        struct BlockingPlugin {
            events: Sender<Event>,
            resume: Receiver<()>,
            loss_attempts: AtomicUsize,
        }

        impl GeyserPlugin for BlockingPlugin {
            fn name(&self) -> &'static str {
                "blocking-leader-test"
            }

            fn leader_transaction_notifications_enabled(&self) -> bool {
                true
            }

            fn notify_leader_bank_status(
                &self,
                _slot: u64,
                _parent: u64,
                bank: u64,
                status: PluginBankStatus,
            ) -> Result<()> {
                self.events.send(Event::Bank(bank, status)).unwrap();
                self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            }

            fn notify_leader_transaction(
                &self,
                info: ReplicaLeaderTransactionInfoVersions,
                _slot: u64,
                bank: u64,
            ) -> Result<()> {
                let ReplicaLeaderTransactionInfoVersions::V0_0_1(info) = info;
                self.events
                    .send(Event::Transaction(bank, info.index, info.status.is_ok()))
                    .unwrap();
                Ok(())
            }

            fn notify_leader_notification_loss(&self, transactions: u64, banks: u64) -> Result<()> {
                self.events.send(Event::Loss(transactions, banks)).unwrap();
                if self.loss_attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err(GeyserPluginError::Custom("retry loss report".into()))
                } else {
                    Ok(())
                }
            }
        }

        agave_logger::setup();
        let (events_sender, events) = unbounded();
        let (resume, resume_receiver) = bounded(1);
        let host = GeyserPluginHost::from_plugins(vec![loaded(BlockingPlugin {
            events: events_sender,
            resume: resume_receiver,
            loss_attempts: AtomicUsize::new(0),
        })]);
        let service =
            LeaderNotificationService::with_capacity(Arc::new(ArcSwap::from_pointee(host)), 1);
        service
            .notifier
            .notify_bank(42, 41, 7, LeaderBankStatus::Created);
        assert_eq!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            Event::Bank(7, PluginBankStatus::Created),
        );

        // Hold the worker inside the plugin so queue overflow is deterministic.
        service.notifier.notify_transactions(transactions(), 0);
        service.notifier.notify_transactions(transactions(), 2);
        service.notifier.notify_bank(
            42,
            41,
            7,
            LeaderBankStatus::Frozen {
                transaction_count: 4,
            },
        );
        assert_eq!(service.notifier.dropped(), (2, 1));
        resume.send(()).unwrap();
        for expected in [
            Event::Loss(2, 1),
            Event::Transaction(7, 0, true),
            Event::Transaction(7, 1, false),
            // No new notifications arrive. The idle poll must retry the failed report.
            Event::Loss(2, 1),
        ] {
            assert_eq!(
                events.recv_timeout(Duration::from_secs(5)).unwrap(),
                expected
            );
        }
        assert!(events.recv_timeout(Duration::from_millis(250)).is_err());
        drop(service);
        assert!(events.try_iter().next().is_none());
    }

    #[test]
    fn worker_panic_is_joined_and_later_notifications_count_as_lost() {
        #[derive(Debug)]
        struct PanickingPlugin;

        impl GeyserPlugin for PanickingPlugin {
            fn name(&self) -> &'static str {
                "panicking-leader-test"
            }

            fn leader_transaction_notifications_enabled(&self) -> bool {
                true
            }

            fn notify_leader_bank_status(
                &self,
                _slot: u64,
                _parent: u64,
                _bank: u64,
                _status: PluginBankStatus,
            ) -> Result<()> {
                panic!("plugin panicked");
            }
        }

        agave_logger::setup();
        let host = GeyserPluginHost::from_plugins(vec![loaded(PanickingPlugin)]);
        let service = LeaderNotificationService::new(Arc::new(ArcSwap::from_pointee(host)));
        let notifier = service.notifier.clone();
        notifier.notify_bank(42, 41, 7, LeaderBankStatus::Created);
        // Drop drains the queued event and joins the panicked thread without propagating it.
        drop(service);
        notifier.notify_transactions(transactions(), 0);
        notifier.notify_bank(
            42,
            41,
            7,
            LeaderBankStatus::Abandoned {
                transaction_count: 2,
            },
        );
        assert_eq!(notifier.dropped(), (2, 1));
    }

    #[test]
    fn worker_drains_queued_events_on_shutdown() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let host = GeyserPluginHost::from_plugins(vec![loaded(Plugin {
            enabled: true,
            fail: false,
            events: events.clone(),
        })]);
        let service = LeaderNotificationService::new(Arc::new(ArcSwap::from_pointee(host)));
        service
            .notifier
            .notify_bank(42, 41, 7, LeaderBankStatus::Created);
        service.notifier.notify_transactions(transactions(), 0);
        service.notifier.notify_bank(
            42,
            41,
            7,
            LeaderBankStatus::Abandoned {
                transaction_count: 2,
            },
        );
        drop(service);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                Event::Bank(7, PluginBankStatus::Created),
                Event::Transaction(7, 0, true),
                Event::Transaction(7, 1, false),
                Event::Bank(
                    7,
                    PluginBankStatus::Abandoned {
                        transaction_count: 2
                    }
                )
            ]
        );
    }
}
