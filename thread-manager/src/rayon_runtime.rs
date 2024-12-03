use {
    crate::policy::CoreAllocation,
    anyhow::Ok,
    serde::{Deserialize, Serialize},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RayonConfig {
    pub worker_threads: usize,
    pub priority: u32,
    pub stack_size_bytes: usize,
    pub core_allocation: CoreAllocation,
}

impl Default for RayonConfig {
    fn default() -> Self {
        Self {
            core_allocation: CoreAllocation::OsDefault,
            worker_threads: 4,
            priority: 0,
            stack_size_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct RayonRuntime {
    pub rayon_pool: rayon::ThreadPool,
    pub config: RayonConfig,
}

impl RayonRuntime {
    fn new(config: RayonConfig) -> anyhow::Result<Self> {
        let policy = config.core_allocation;
        let rayon_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.worker_threads)
            .start_handler(move |idx| {
                affinity::set_thread_affinity([1, 2, 3]).unwrap();
            })
            .build()?;
        Ok(Self { rayon_pool, config })
    }
}
