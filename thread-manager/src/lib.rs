use {
    anyhow::Ok,
    serde::{Deserialize, Serialize},
    std::collections::HashMap,
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
pub type ConstString = Box<str>;

#[derive(Default, Debug)]
pub struct ThreadManager {
    pub tokio_runtimes: HashMap<ConstString, TokioRuntime>,
    pub tokio_runtime_mapping: HashMap<ConstString, ConstString>,

    pub native_thread_runtimes: HashMap<ConstString, NativeThreadRuntime>,
    pub native_runtime_mapping: HashMap<ConstString, ConstString>,

    pub rayon_runtimes: HashMap<ConstString, RayonRuntime>,
    pub rayon_runtime_mapping: HashMap<ConstString, ConstString>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeManagerConfig {
    pub native_configs: HashMap<String, NativeConfig>,
    pub native_runtime_mapping: HashMap<String, String>,

    pub rayon_configs: HashMap<String, RayonConfig>,
    pub rayon_runtime_mapping: HashMap<String, String>,

    pub tokio_configs: HashMap<String, TokioConfig>,
    pub tokio_runtime_mapping: HashMap<String, String>,

    pub default_core_allocation: CoreAllocation,
}

impl ThreadManager {
    pub fn get_native(&self, name: &str) -> Option<&NativeThreadRuntime> {
        let n = self.native_runtime_mapping.get(name)?;
        self.native_thread_runtimes.get(n)
    }
    pub fn get_rayon(&self, name: &str) -> Option<&RayonRuntime> {
        let n = self.rayon_runtime_mapping.get(name)?;
        self.rayon_runtimes.get(n)
    }
    pub fn get_tokio(&self, name: &str) -> Option<&TokioRuntime> {
        let n = self.tokio_runtime_mapping.get(name)?;
        self.tokio_runtimes.get(n)
    }
    pub fn set_process_affinity(config: &RuntimeManagerConfig) -> anyhow::Result<Vec<usize>> {
        let chosen_cores_mask = config.default_core_allocation.as_core_mask_vector();

        crate::policy::set_thread_affinity(&chosen_cores_mask);
        Ok(chosen_cores_mask)
    }

    /// Populates mappings with copies of config names, overrides as appropriate
    fn populate_mappings(&mut self, config: &RuntimeManagerConfig) {
        //TODO: this should probably be cleaned up with a macro at some point...

        for name in config.native_configs.keys() {
            self.native_runtime_mapping
                .insert(name.clone().into_boxed_str(), name.clone().into_boxed_str());
        }
        for (k, v) in config.native_runtime_mapping.iter() {
            self.native_runtime_mapping
                .insert(k.clone().into_boxed_str(), v.clone().into_boxed_str());
        }

        for name in config.tokio_configs.keys() {
            self.tokio_runtime_mapping
                .insert(name.clone().into_boxed_str(), name.clone().into_boxed_str());
        }
        for (k, v) in config.tokio_runtime_mapping.iter() {
            self.tokio_runtime_mapping
                .insert(k.clone().into_boxed_str(), v.clone().into_boxed_str());
        }

        for name in config.rayon_configs.keys() {
            self.rayon_runtime_mapping
                .insert(name.clone().into_boxed_str(), name.clone().into_boxed_str());
        }
        for (k, v) in config.rayon_runtime_mapping.iter() {
            self.rayon_runtime_mapping
                .insert(k.clone().into_boxed_str(), v.clone().into_boxed_str());
        }
    }
    pub fn new(config: RuntimeManagerConfig) -> anyhow::Result<Self> {
        let mut core_allocations = HashMap::<ConstString, Vec<usize>>::new();
        Self::set_process_affinity(&config)?;
        let mut manager = Self::default();
        manager.populate_mappings(&config);
        for (name, cfg) in config.native_configs.iter() {
            let nrt = NativeThreadRuntime::new(name.clone(), cfg.clone());
            manager
                .native_thread_runtimes
                .insert(name.clone().into_boxed_str(), nrt);
        }
        for (name, cfg) in config.rayon_configs.iter() {
            let rrt = RayonRuntime::new(name.clone(), cfg.clone())?;
            manager
                .rayon_runtimes
                .insert(name.clone().into_boxed_str(), rrt);
        }

        for (name, cfg) in config.tokio_configs.iter() {
            let tokiort = TokioRuntime::new(name.clone(), cfg.clone())?;

            core_allocations.insert(
                name.clone().into_boxed_str(),
                cfg.core_allocation.as_core_mask_vector(),
            );
            manager
                .tokio_runtimes
                .insert(name.clone().into_boxed_str(), tokiort);
        }
        Ok(manager)
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{CoreAllocation, NativeConfig, RayonConfig, RuntimeManagerConfig, ThreadManager},
        std::{collections::HashMap, io::Read},
    };

    #[test]
    fn configtest() {
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
            let cfg: RuntimeManagerConfig = toml::from_str(&buf).unwrap();
            println!("{:?}", cfg);
        }
    }
    // Nobody runs Agave on windows, and on Mac we can not set mask affinity without patching external crate
    #[cfg(target_os = "linux")]
    fn validate_affinity(expect_cores: &[usize], error_msg: &str) {
        let aff = affinity::get_thread_affinity().unwrap();
        assert_eq!(aff, expect_cores, "{}", error_msg);
    }
    #[cfg(not(target_os = "linux"))]
    fn validate_affinity(_expect_cores: &[usize], _error_msg: &str) {}

    #[test]
    fn process_affinity() {
        let conf = RuntimeManagerConfig {
            native_configs: HashMap::from([(
                "pool1".to_owned(),
                NativeConfig {
                    core_allocation: CoreAllocation::DedicatedCoreSet { min: 0, max: 4 },
                    max_threads: 5,
                    priority: 0,
                    ..Default::default()
                },
            )]),
            default_core_allocation: CoreAllocation::DedicatedCoreSet { min: 4, max: 8 },
            native_runtime_mapping: HashMap::from([("test".to_owned(), "pool1".to_owned())]),
            ..Default::default()
        };

        let rtm = ThreadManager::new(conf).unwrap();
        let r = rtm.get_native("test").unwrap();

        let t2 = r
            .spawn(|| {
                validate_affinity(&[0, 1, 2, 3], "Managed thread allocation should be 0-3");
            })
            .unwrap();

        let t = std::thread::spawn(|| {
            validate_affinity(&[4, 5, 6, 7], "Default thread allocation should be 4-7");

            let tt = std::thread::spawn(|| {
                validate_affinity(
                    &[4, 5, 6, 7],
                    "Nested thread allocation should still be 4-7",
                );
            });
            tt.join().unwrap();
        });
        t.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn rayon_affinity() {
        let conf = RuntimeManagerConfig {
            native_configs: HashMap::from([(
                "pool1".to_owned(),
                NativeConfig {
                    core_allocation: CoreAllocation::DedicatedCoreSet { min: 0, max: 4 },
                    max_threads: 5,
                    priority: 0,
                    ..Default::default()
                },
            )]),
            rayon_configs: HashMap::from([(
                "rayon1".to_owned(),
                RayonConfig {
                    core_allocation: CoreAllocation::DedicatedCoreSet { min: 1, max: 4 },
                    worker_threads: 3,
                    priority: 0,
                    ..Default::default()
                },
            )]),
            default_core_allocation: CoreAllocation::DedicatedCoreSet { min: 4, max: 8 },
            native_runtime_mapping: HashMap::from([("test".to_owned(), "pool1".to_owned())]),

            rayon_runtime_mapping: HashMap::from([("test".to_owned(), "rayon1".to_owned())]),
            ..Default::default()
        };

        let rtm = ThreadManager::new(conf).unwrap();
        let r = rtm.get_native("test").unwrap();

        let t2 = r
            .spawn(|| {
                validate_affinity(&[0, 1, 2, 3], "Managed thread allocation should be 0-3");
            })
            .unwrap();
        let rrt = rtm.get_rayon("test").unwrap();

        let t = std::thread::spawn(|| {
            validate_affinity(&[4, 5, 6, 7], "Default thread allocation should be 4-7");

            let tt = std::thread::spawn(|| {
                validate_affinity(
                    &[4, 5, 6, 7],
                    "Nested thread allocation should still be 4-7",
                );
            });
            tt.join().unwrap();
        });
        let _rr = rrt.rayon_pool.broadcast(|ctx| {
            println!("Rayon thread {} reporting", ctx.index());
            validate_affinity(&[1, 2, 3], "Rayon thread allocation should still be 1-3");
        });
        t.join().unwrap();
        t2.join().unwrap();
    }
}
