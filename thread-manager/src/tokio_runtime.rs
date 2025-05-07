use {
    crate::{
        policy::{apply_policy, parse_policy, CoreAllocation},
        MAX_THREAD_NAME_CHARS,
    },
    serde::{Deserialize, Serialize},
    solana_metrics::datapoint_info,
    std::{
        ops::Deref,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    },
    thread_priority::ThreadExt,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TokioConfig {
    ///number of worker threads tokio is allowed to spawn
    pub worker_threads: usize,
    ///max number of blocking threads tokio is allowed to spawn
    pub max_blocking_threads: usize,
    /// Priority in range 0..99
    pub priority: u8,
    pub policy: String,
    pub stack_size_bytes: usize,
    pub event_interval: u32,
    pub core_allocation: CoreAllocation,
}

impl Default for TokioConfig {
    fn default() -> Self {
        Self {
            core_allocation: CoreAllocation::OsDefault,
            worker_threads: 8,
            max_blocking_threads: 1,
            priority: crate::policy::DEFAULT_PRIORITY,
            policy: "OTHER".to_owned(),
            stack_size_bytes: 2 * 1024 * 1024,
            event_interval: 61,
        }
    }
}

#[derive(Debug)]
pub struct TokioRuntime {
    pub tokio: tokio::runtime::Runtime,
    pub config: TokioConfig,
    pub counters: Arc<ThreadCounters>,
}

impl Deref for TokioRuntime {
    type Target = tokio::runtime::Runtime;

    fn deref(&self) -> &Self::Target {
        &self.tokio
    }
}

impl TokioRuntime {
    /// Starts the metrics sampling task on the runtime to monitor
    /// how many workers are busy doing useful things.
    pub fn start_metrics_sampling(&self, period: Duration) {
        let counters = self.counters.clone();
        self.tokio.spawn(metrics_sampler(counters, period));
    }

    pub fn new(name: String, cfg: TokioConfig) -> anyhow::Result<Self> {
        debug_assert!(name.len() < MAX_THREAD_NAME_CHARS, "Thread name too long");
        let num_workers = if cfg.worker_threads == 0 {
            num_cpus::get()
        } else {
            cfg.worker_threads
        };
        let chosen_cores_mask = cfg.core_allocation.as_core_mask_vector();

        let base_name = name.clone();
        let mut builder = match num_workers {
            1 => tokio::runtime::Builder::new_current_thread(),
            _ => {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(num_workers);
                builder
            }
        };
        let atomic_id: AtomicUsize = AtomicUsize::new(0);

        let counters = Arc::new(ThreadCounters {
            // no workaround, metrics crate will only consume 'static str
            namespace: format!("thread-manager-tokio-{}", &base_name).leak(),
            total_threads_cnt: cfg.worker_threads as u64,
            active_threads_cnt: AtomicU64::new(
                (num_workers.wrapping_add(cfg.max_blocking_threads)) as u64,
            ),
        });
        builder
            .event_interval(cfg.event_interval)
            .thread_name_fn(move || {
                let id = atomic_id.fetch_add(1, Ordering::Relaxed);
                format!("{}-{}", base_name, id)
            })
            .on_thread_park({
                let counters = counters.clone();
                move || {
                    counters.on_park();
                }
            })
            .on_thread_unpark({
                let counters = counters.clone();
                move || {
                    counters.on_unpark();
                }
            })
            .thread_stack_size(cfg.stack_size_bytes)
            .enable_all()
            .max_blocking_threads(cfg.max_blocking_threads);

        //keep borrow checker happy and move these things into the closure
        let c = cfg.clone();
        let chosen_cores_mask = Mutex::new(chosen_cores_mask);
        builder.on_thread_start(move || {
            let cur_thread = std::thread::current();
            let _tid = cur_thread
                .get_native_id()
                .expect("Can not get thread id for newly created thread");

            apply_policy(
                &c.core_allocation,
                parse_policy(&c.policy),
                c.priority,
                &chosen_cores_mask,
            );
        });
        Ok(TokioRuntime {
            tokio: builder.build()?,
            config: cfg.clone(),
            counters,
        })
    }

    /// Makes test runtime with 2 threads, only for unittests
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests() -> Self {
        let cfg = TokioConfig {
            worker_threads: 2,
            ..Default::default()
        };
        TokioRuntime::new("solNetTest".to_owned(), cfg.clone())
            .expect("Failed to create Tokio runtime for tests")
    }
}

/// Internal counters to keep track of worker pool utilization
#[derive(Debug)]
pub struct ThreadCounters {
    pub namespace: &'static str,
    pub total_threads_cnt: u64,
    pub active_threads_cnt: AtomicU64,
}

impl ThreadCounters {
    pub fn on_park(&self) {
        self.active_threads_cnt.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn on_unpark(&self) {
        self.active_threads_cnt.fetch_add(1, Ordering::Relaxed);
    }
}

async fn metrics_sampler(counters: Arc<ThreadCounters>, period: Duration) {
    let mut interval = tokio::time::interval(period);
    loop {
        interval.tick().await;
        let active = counters.active_threads_cnt.load(Ordering::Relaxed) as i64;
        let parked = (counters.total_threads_cnt as i64).saturating_sub(active);
        datapoint_info!(
            counters.namespace,
            ("threads_parked", parked, i64),
            ("threads_active", active, i64),
        );
    }
}
