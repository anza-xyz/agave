use {
    crate::{
        policy::{apply_policy, parse_policy, CoreAllocation},
        MAX_THREAD_NAME_CHARS,
    },
    anyhow::Ok,
    serde::{Deserialize, Serialize},
    std::{
        ops::Deref,
        sync::{Arc, Mutex},
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RayonConfig {
    pub worker_threads: usize,
    /// Priority in range 0..99
    pub priority: u8,
    pub policy: String,
    pub stack_size_bytes: usize,
    pub core_allocation: CoreAllocation,
}

impl Default for RayonConfig {
    fn default() -> Self {
        Self {
            core_allocation: CoreAllocation::OsDefault,
            worker_threads: 16,
            priority: crate::policy::DEFAULT_PRIORITY,
            policy: "BATCH".to_owned(),
            stack_size_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct RayonRuntimeInner {
    pub rayon_pool: rayon::ThreadPool,
    pub config: RayonConfig,
}
impl Deref for RayonRuntimeInner {
    type Target = rayon::ThreadPool;

    fn deref(&self) -> &Self::Target {
        &self.rayon_pool
    }
}

#[derive(Debug, Clone)]
pub struct RayonRuntime {
    inner: Arc<RayonRuntimeInner>,
}

impl Deref for RayonRuntime {
    type Target = RayonRuntimeInner;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl RayonRuntime {
    pub fn new(name: String, config: RayonConfig) -> anyhow::Result<Self> {
        debug_assert!(name.len() < MAX_THREAD_NAME_CHARS, "Thread name too long");
        let core_allocation = config.core_allocation.clone();
        let chosen_cores_mask = Mutex::new(core_allocation.as_core_mask_vector());
        let priority = config.priority;
        let policy = parse_policy(&config.policy);
        let rayon_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.worker_threads)
            .thread_name(move |i| format!("{}_{}", &name, i))
            .stack_size(config.stack_size_bytes)
            .start_handler(move |_idx| {
                apply_policy(&core_allocation, policy, priority, &chosen_cores_mask);
            })
            .build()?;
        Ok(Self {
            inner: Arc::new(RayonRuntimeInner { rayon_pool, config }),
        })
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(name: &str) -> Self {
        Self::new(name.to_owned(), RayonConfig::default())
            .expect("Failed to create rayon runtime for tests")
    }
}
