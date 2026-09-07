//! Owns the dashboard's threads and ties the collector to the server.
//!
//! The server and a boot-progress thread come up near the top of validator
//! startup, so a snapshot download or ledger replay is visible rather than
//! blank; the collector attaches once bank forks and the blockstore exist.
//! Sampling runs on two threads, the collector for slots and the meters for the
//! once-a-second readings, so a slow blockstore read does not stall every
//! panel.

use {
    crate::{
        collect::{Collector, EpochInfo, system_time_nanos},
        config::DashboardConfig,
        context::{DashboardContext, StartProgress},
        history::{PACKED_SLOTS, SlotHistory},
        meters::{METER_INTERVAL, Meters},
        metrics_tap::MetricsTap,
        proto::{Publisher, TOPIC_SUMMARY},
        server,
        startup::StartupPublisher,
        tips::TipMeter,
        validator_info::ValidatorInfoCache,
    },
    solana_pubkey::Pubkey,
    std::{
        io,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, SystemTime},
    },
    tokio::{net::TcpListener, runtime::Builder},
};

/// Worker threads the dashboard's runtime is allowed. `Runtime::new()` takes
/// one per core, in the same process as replay, banking and PoH. The
/// dashboard's work is almost all socket writes, so two is generous; what this
/// bounds is the hostile case.
const RUNTIME_THREADS: usize = 2;

/// How often the boot thread samples the startup phase. Phases last seconds at
/// least.
const BOOT_POLL: Duration = Duration::from_millis(250);

/// How often the collector samples. Five times a second is fast enough that a
/// slot never passes between two samples, which the slot ring depends on.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

pub struct DashboardService {
    exit: Arc<AtomicBool>,
    /// Stops the boot thread once the collector has taken over reporting.
    attached: Arc<AtomicBool>,
    publisher: Arc<Publisher>,
    /// Retained from `start` and handed to the collector at `attach`.
    startup_progress: StartProgress,
    /// When the dashboard came up, which is as near to when the process did as
    /// anything reports. For the uptime readout.
    started: SystemTime,
    /// Counters lifted from the measurements the validator submits about
    /// itself, watched from `start` so the boot sequence is counted too.
    metrics_tap: Arc<MetricsTap>,
    /// Times the boot phases from `start` and is handed to the collector at
    /// `attach`, so the record survives the handover.
    startup: Arc<std::sync::Mutex<StartupPublisher>>,
    /// The jito tip settings, retained from `start` until the collector exists.
    tip_payment_program_id: Option<Pubkey>,
    commission_bps: Option<u16>,
    /// The packed slot history, shared with the server, which answers range
    /// queries out of it. Allocated here because the server starts before the
    /// collector.
    history: Arc<RwLock<SlotHistory>>,
    /// Validator names and icons, shared with the server, which answers a
    /// request for the whole table out of it.
    info_cache: Arc<RwLock<ValidatorInfoCache>>,
    /// This epoch and the one before it, shared with the server, which answers
    /// a query for either out of it.
    epochs: Arc<RwLock<Vec<EpochInfo>>>,
    server: Option<JoinHandle<()>>,
    boot: Option<JoinHandle<()>>,
    collector: Option<JoinHandle<()>>,
    meters: Option<JoinHandle<()>>,
    info_loader: Option<JoinHandle<()>>,
}

impl DashboardService {
    /// Binds the listener and begins serving startup progress. The only error is a
    /// failure to bind, so a misconfigured dashboard fails loudly at boot. Call
    /// [`DashboardService::attach`] once the validator is assembled.
    pub fn start(
        config: DashboardConfig,
        startup_progress: StartProgress,
        exit: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let publisher = Arc::new(Publisher::new());
        let started = SystemTime::now();
        publisher.publish(
            TOPIC_SUMMARY,
            "startup_time_nanos",
            &system_time_nanos(started),
        );
        // Installed with the service rather than the collector, so points submitted
        // during the boot sequence are counted too.
        let metrics_tap = MetricsTap::install();
        let history = Arc::new(RwLock::new(SlotHistory::new(PACKED_SLOTS)));
        // Here for the same reason: the server answers a request out of it and starts
        // first.
        let info_cache = Arc::new(RwLock::new(ValidatorInfoCache::default()));
        // This epoch and the one before it, for pages reading back across the
        // boundary.
        let epochs: Arc<RwLock<Vec<EpochInfo>>> = Arc::new(RwLock::new(Vec::new()));
        let attached = Arc::new(AtomicBool::new(false));
        let startup = Arc::new(std::sync::Mutex::new(StartupPublisher::default()));

        let runtime = Builder::new_multi_thread()
            .worker_threads(RUNTIME_THREADS)
            .thread_name("solDashRt")
            .enable_all()
            .build()?;
        let listener = runtime.block_on(async { TcpListener::bind(config.listen_addr).await })?;
        log::info!(
            "dashboard: listening on http://{} (websocket at /websocket)",
            config.listen_addr
        );

        let allowed_hosts: Arc<[String]> = config.allowed_hosts.clone().into();
        log::info!("dashboard: answering to hosts {:?}", config.allowed_hosts);

        let server = {
            let publisher = publisher.clone();
            let history = history.clone();
            let info_cache = info_cache.clone();
            let epochs = epochs.clone();
            let exit = exit.clone();
            thread::Builder::new()
                .name("solDashSrv".to_string())
                .spawn(move || {
                    runtime.block_on(async move {
                        tokio::select! {
                            _ = server::serve(listener, publisher, history, info_cache, epochs, allowed_hosts) => {}
                            _ = wait_for_exit(exit) => {}
                        }
                    });
                })?
        };

        let boot = {
            let publisher = publisher.clone();
            let exit = exit.clone();
            let attached = attached.clone();
            let startup_progress = startup_progress.clone();
            let startup = startup.clone();
            thread::Builder::new()
                .name("solDashBoot".to_string())
                .spawn(move || {
                    while !attached.load(Ordering::Relaxed) && !exit.load(Ordering::Relaxed) {
                        let progress = *startup_progress.read().unwrap();
                        startup.lock().unwrap().publish(&publisher, progress);
                        thread::sleep(BOOT_POLL);
                    }
                })?
        };

        Ok(Self {
            exit,
            attached,
            publisher,
            startup_progress,
            started,
            metrics_tap,
            startup,
            tip_payment_program_id: config.tip_payment_program_id,
            commission_bps: config.commission_bps,
            history,
            info_cache,
            epochs,
            server: Some(server),
            boot: Some(boot),
            collector: None,
            meters: None,
            info_loader: None,
        })
    }

    /// Starts the collector against a fully assembled validator. Both threads
    /// publish startup progress through the same [`StartupPublisher`], so the
    /// handover is invisible to a client.
    pub fn attach(&mut self, context: DashboardContext) -> io::Result<()> {
        let info_cache = self.info_cache.clone();

        // Validator names are read once here, off the collector's thread, and the
        // cache lock is taken only to merge the result. Whether it finds anything
        // depends on how the validator was started, which `scan_all` logs.
        self.info_loader = Some({
            let context = context.clone();
            let info_cache = info_cache.clone();
            thread::Builder::new()
                .name("solDashInfo".to_string())
                .spawn(move || {
                    let bank = context.bank_forks.read().unwrap().root_bank();
                    let started = std::time::Instant::now();
                    let entries = crate::validator_info::scan_all(&bank);
                    let found = entries.len();
                    let loaded = info_cache.write().unwrap().merge(entries);
                    log::info!(
                        "dashboard: read validator info in {:?}, {found} accounts, {loaded} cached",
                        started.elapsed()
                    );
                })?
        });

        self.meters = Some({
            let exit = self.exit.clone();
            let publisher = self.publisher.clone();
            let startup_progress = self.startup_progress.clone();
            let started = self.started;
            let context = context.clone();
            let metrics_tap = self.metrics_tap.clone();
            thread::Builder::new()
                .name("solDashMeter".to_string())
                .spawn(move || {
                    let mut meters =
                        Meters::new(context, publisher, startup_progress, started, metrics_tap);
                    while !exit.load(Ordering::Relaxed) {
                        meters.tick();
                        thread::sleep(METER_INTERVAL);
                    }
                })?
        });

        self.collector = Some({
            let exit = self.exit.clone();
            let publisher = self.publisher.clone();
            let history = self.history.clone();
            let epochs = self.epochs.clone();
            let startup_progress = self.startup_progress.clone();
            let startup = self.startup.clone();
            let metrics_tap = self.metrics_tap.clone();
            // Derived once here rather than per tick. Absent on a validator
            // with no tip payment program, and then no tips are read at all.
            let tips = self.tip_payment_program_id.as_ref().map(TipMeter::new);
            let commission_bps = self.commission_bps;
            thread::Builder::new()
                .name("solDashColl".to_string())
                .spawn(move || {
                    let mut collector = Collector::new(
                        context,
                        publisher,
                        info_cache,
                        history,
                        epochs,
                        startup_progress,
                        startup,
                        metrics_tap,
                        tips,
                        commission_bps,
                    );
                    collector.publish_static();
                    while !exit.load(Ordering::Relaxed) {
                        collector.tick();
                        thread::sleep(POLL_INTERVAL);
                    }
                })?
        });

        // Set last: the boot thread must not stop before the collector exists,
        // or startup progress would go stale in the gap.
        self.attached.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn join(mut self) -> thread::Result<()> {
        for handle in [
            self.collector.take(),
            self.meters.take(),
            self.boot.take(),
            self.server.take(),
            self.info_loader.take(),
        ]
        .into_iter()
        .flatten()
        {
            handle.join()?;
        }
        Ok(())
    }
}

async fn wait_for_exit(exit: Arc<AtomicBool>) {
    while !exit.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::fixture::fixture, solana_core::validator::ValidatorStartProgress,
        std::sync::mpsc,
    };

    #[test]
    fn test_service_exit() {
        let harness = fixture();
        let exit = Arc::new(AtomicBool::new(false));
        let config = DashboardConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            allowed_hosts: Vec::new(),
            tip_payment_program_id: None,
            commission_bps: None,
        };
        let mut service = DashboardService::start(
            config,
            Arc::new(RwLock::new(ValidatorStartProgress::Running)),
            exit.clone(),
        )
        .unwrap();
        service.attach(harness.ctx.clone()).unwrap();

        exit.store(true, Ordering::Relaxed);
        // Joined on a helper thread so a regression fails instead of hanging.
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(service.join()));
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the dashboard did not stop within ten seconds")
            .unwrap();
    }
}
