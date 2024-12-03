use {
    crate::policy::{apply_policy, CoreAllocation},
    anyhow::Ok,
    serde::{Deserialize, Serialize},
    std::sync::Mutex,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RayonConfig {
    pub worker_threads: usize,
    pub priority: u8,
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
    pub fn new(config: RayonConfig) -> anyhow::Result<Self> {
        let policy = config.core_allocation.clone();
        let chosen_cores_mask = Mutex::new(policy.as_core_mask_vector());
        let priority = config.priority;
        let rayon_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.worker_threads)
            .start_handler(move |_idx| {
                apply_policy(&policy, priority, &chosen_cores_mask);
            })
            .build()?;
        Ok(Self { rayon_pool, config })
    }
}
