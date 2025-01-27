use {
    anyhow::Ok,
    serde::{Deserialize, Serialize},
    std::{collections::HashMap, ops::Deref, sync::Arc},
};

pub mod native_thread_runtime;
pub mod policy;
pub mod rayon_runtime;
pub mod tokio_runtime;

pub use {
    native_thread_runtime::{JoinHandle, NativeConfig, NativeThreadRuntime},
    policy::CoreAllocation,
    rayon_runtime::{RayonConfig, RayonRuntime},
    tokio_runtime::{TokioConfig, TokioRuntime},
};

pub const MAX_THREAD_NAME_CHARS: usize = 12;

#[derive(Default, Debug)]
pub struct ThreadManagerInner {
    pub tokio_runtimes: HashMap<String, TokioRuntime>,
    pub tokio_runtime_mapping: HashMap<String, String>,

    pub native_thread_runtimes: HashMap<String, NativeThreadRuntime>,
    pub native_runtime_mapping: HashMap<String, String>,

    pub rayon_runtimes: HashMap<String, RayonRuntime>,
    pub rayon_runtime_mapping: HashMap<String, String>,
}

impl ThreadManagerInner {
    /// Populates mappings with copies of config names, overrides as appropriate
    fn populate_mappings(&mut self, config: &ThreadManagerConfig) {
        //TODO: this should probably be cleaned up with a macro at some point...

        for name in config.native_configs.keys() {
            self.native_runtime_mapping
                .insert(name.clone(), name.clone());
        }
        for (k, v) in config.native_runtime_mapping.iter() {
            self.native_runtime_mapping.insert(k.clone(), v.clone());
        }

        for name in config.tokio_configs.keys() {
            self.tokio_runtime_mapping
                .insert(name.clone(), name.clone());
        }
        for (k, v) in config.tokio_runtime_mapping.iter() {
            self.tokio_runtime_mapping.insert(k.clone(), v.clone());
        }

        for name in config.rayon_configs.keys() {
            self.rayon_runtime_mapping
                .insert(name.clone(), name.clone());
        }
        for (k, v) in config.rayon_runtime_mapping.iter() {
            self.rayon_runtime_mapping.insert(k.clone(), v.clone());
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct ThreadManager {
    inner: Arc<ThreadManagerInner>,
}

impl Deref for ThreadManager {
    type Target = ThreadManagerInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadManagerConfig {
    pub native_configs: HashMap<String, NativeConfig>,
    pub native_runtime_mapping: HashMap<String, String>,

    pub rayon_configs: HashMap<String, RayonConfig>,
    pub rayon_runtime_mapping: HashMap<String, String>,

    pub tokio_configs: HashMap<String, TokioConfig>,
    pub tokio_runtime_mapping: HashMap<String, String>,

    pub default_core_allocation: CoreAllocation,
}

impl Default for ThreadManagerConfig {
    fn default() -> Self {
        Self {
            native_configs: HashMap::from([("default".to_owned(), NativeConfig::default())]),
            native_runtime_mapping: HashMap::new(),
            rayon_configs: HashMap::from([("default".to_owned(), RayonConfig::default())]),
            rayon_runtime_mapping: HashMap::new(),
            tokio_configs: HashMap::from([("default".to_owned(), TokioConfig::default())]),
            tokio_runtime_mapping: HashMap::new(),
            default_core_allocation: CoreAllocation::OsDefault,
        }
    }
}

impl ThreadManager {
    /// Will lookup a runtime by given name. If not found, will try to lookup by name "default". If all fails, returns None.
    fn lookup<'a, T>(
        &'a self,
        name: &str,
        mapping: &HashMap<String, String>,
        runtimes: &'a HashMap<String, T>,
    ) -> Option<&'a T> {
        match mapping.get(name) {
            Some(n) => runtimes.get(n),
            None => match mapping.get("default") {
                Some(n) => {
                    log::warn!("Falling back to default runtime for {name}");
                    runtimes.get(n)
                }
                None => None,
            },
        }
    }

    pub fn try_get_native(&self, name: &str) -> Option<&NativeThreadRuntime> {
        self.lookup(
            name,
            &self.native_runtime_mapping,
            &self.native_thread_runtimes,
        )
    }
    pub fn get_native(&self, name: &str) -> &NativeThreadRuntime {
        if let Some(runtime) = self.try_get_native(name) {
            runtime
        } else {
            panic!("Native thread pool for {name} can not be found!");
        }
    }

    pub fn try_get_rayon(&self, name: &str) -> Option<&RayonRuntime> {
        self.lookup(name, &self.rayon_runtime_mapping, &self.rayon_runtimes)
    }

    pub fn get_rayon(&self, name: &str) -> &RayonRuntime {
        if let Some(runtime) = self.try_get_rayon(name) {
            runtime
        } else {
            panic!("Rayon thread pool for {name} can not be found!");
        }
    }

    pub fn try_get_tokio(&self, name: &str) -> Option<&TokioRuntime> {
        self.lookup(name, &self.tokio_runtime_mapping, &self.tokio_runtimes)
    }

    pub fn get_tokio(&self, name: &str) -> &TokioRuntime {
        if let Some(runtime) = self.try_get_tokio(name) {
            runtime
        } else {
            panic!("Tokio thread pool for {name} can not be found!");
        }
    }

    pub fn set_process_affinity(config: &ThreadManagerConfig) -> anyhow::Result<Vec<usize>> {
        let chosen_cores_mask = config.default_core_allocation.as_core_mask_vector();
        crate::policy::set_thread_affinity(&chosen_cores_mask);
        Ok(chosen_cores_mask)
    }

    pub fn new(config: ThreadManagerConfig) -> anyhow::Result<Self> {
        let mut core_allocations = HashMap::<String, Vec<usize>>::new();
        Self::set_process_affinity(&config)?;
        let mut manager = ThreadManagerInner::default();
        manager.populate_mappings(&config);
        for (name, cfg) in config.native_configs.iter() {
            let nrt = NativeThreadRuntime::new(name.clone(), cfg.clone());
            manager.native_thread_runtimes.insert(name.clone(), nrt);
        }
        for (name, cfg) in config.rayon_configs.iter() {
            let rrt = RayonRuntime::new(name.clone(), cfg.clone())?;
            manager.rayon_runtimes.insert(name.clone(), rrt);
        }

        for (name, cfg) in config.tokio_configs.iter() {
            let tokiort = TokioRuntime::new(name.clone(), cfg.clone())?;

            core_allocations.insert(name.clone(), cfg.core_allocation.as_core_mask_vector());
            manager.tokio_runtimes.insert(name.clone(), tokiort);
        }
        Ok(Self {
            inner: Arc::new(manager),
        })
    }
}

#[cfg(test)]
mod tests {
    use {crate::ThreadManagerConfig, std::io::Read};
    #[cfg(target_os = "linux")]
    use {
        crate::{CoreAllocation, NativeConfig, RayonConfig, ThreadManager},
        std::collections::HashMap,
    };

    #[test]
    fn test_config_files() {
        let experiments = [
            "examples/core_contention_dedicated_set.toml",
            "examples/core_contention_contending_set.toml",
        ];

        for exp in experiments {
            println!("Loading config {exp}");
            let mut conffile = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            conffile.push(exp);
            let mut buf = String::new();
            std::fs::File::open(conffile)
                .unwrap()
                .read_to_string(&mut buf)
                .unwrap();
            let cfg: ThreadManagerConfig = toml::from_str(&buf).unwrap();
            println!("{:?}", cfg);
        }
    }
    // Nobody runs Agave on windows, and on Mac we can not set mask affinity without patching external crate
    #[cfg(target_os = "linux")]
    fn validate_affinity(expect_cores: &[usize], error_msg: &str) {
        let affinity = affinity::get_thread_affinity().unwrap();
        assert_eq!(affinity, expect_cores, "{}", error_msg);
    }
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore] //test ignored for now as thread priority requires kernel support and extra permissions
    fn test_thread_priority() {
        let priority_high = 10;
        let priority_default = crate::policy::DEFAULT_PRIORITY;
        let priority_low = 1;
        let conf = ThreadManagerConfig {
            native_configs: HashMap::from([
                (
                    "high".to_owned(),
                    NativeConfig {
                        priority: priority_high,
                        ..Default::default()
                    },
                ),
                (
                    "default".to_owned(),
                    NativeConfig {
                        ..Default::default()
                    },
                ),
                (
                    "low".to_owned(),
                    NativeConfig {
                        priority: priority_low,
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };

        let manager = ThreadManager::new(conf).unwrap();
        let high = manager.get_native("high");
        let low = manager.get_native("low");
        let default = manager.get_native("default");

        high.spawn(move || {
            let prio =
                thread_priority::get_thread_priority(thread_priority::thread_native_id()).unwrap();
            assert_eq!(
                prio,
                thread_priority::ThreadPriority::Crossplatform((priority_high).try_into().unwrap())
            );
        })
        .unwrap()
        .join()
        .unwrap();
        low.spawn(move || {
            let prio =
                thread_priority::get_thread_priority(thread_priority::thread_native_id()).unwrap();
            assert_eq!(
                prio,
                thread_priority::ThreadPriority::Crossplatform((priority_low).try_into().unwrap())
            );
        })
        .unwrap()
        .join()
        .unwrap();
        default
            .spawn(move || {
                let prio =
                    thread_priority::get_thread_priority(thread_priority::thread_native_id())
                        .unwrap();
                assert_eq!(
                    prio,
                    thread_priority::ThreadPriority::Crossplatform(
                        (priority_default).try_into().unwrap()
                    )
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_process_affinity() {
        let conf = ThreadManagerConfig {
            native_configs: HashMap::from([(
                "pool1".to_owned(),
                NativeConfig {
                    core_allocation: CoreAllocation::DedicatedCoreSet { min: 0, max: 4 },
                    max_threads: 5,
                    ..Default::default()
                },
            )]),
            default_core_allocation: CoreAllocation::DedicatedCoreSet { min: 4, max: 8 },
            native_runtime_mapping: HashMap::from([("test".to_owned(), "pool1".to_owned())]),
            ..Default::default()
        };

        let manager = ThreadManager::new(conf).unwrap();
        let runtime = manager.get_native("test");

        let thread1 = runtime
            .spawn(|| {
                validate_affinity(&[0, 1, 2, 3], "Managed thread allocation should be 0-3");
            })
            .unwrap();

        let thread2 = std::thread::spawn(|| {
            validate_affinity(&[4, 5, 6, 7], "Default thread allocation should be 4-7");

            let inner_thread = std::thread::spawn(|| {
                validate_affinity(
                    &[4, 5, 6, 7],
                    "Nested thread allocation should still be 4-7",
                );
            });
            inner_thread.join().unwrap();
        });
        thread1.join().unwrap();
        thread2.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_rayon_affinity() {
        let conf = ThreadManagerConfig {
            rayon_configs: HashMap::from([(
                "test".to_owned(),
                RayonConfig {
                    core_allocation: CoreAllocation::DedicatedCoreSet { min: 1, max: 4 },
                    worker_threads: 3,
                    ..Default::default()
                },
            )]),
            default_core_allocation: CoreAllocation::DedicatedCoreSet { min: 4, max: 8 },

            ..Default::default()
        };

        let manager = ThreadManager::new(conf).unwrap();
        let rayon_runtime = manager.get_rayon("test");

        let _rr = rayon_runtime.rayon_pool.broadcast(|ctx| {
            println!("Rayon thread {} reporting", ctx.index());
            validate_affinity(&[1, 2, 3], "Rayon thread allocation should still be 1-3");
        });
    }
}
