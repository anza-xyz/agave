use {
    log::{debug, error, warn},
    std::{
        collections::HashMap,
        ops::Deref,
        sync::{atomic::Ordering, Arc},
    },
};

pub mod config;
pub mod native_thread_runtime;
pub mod policy;
pub mod rayon_runtime;
pub mod tokio_runtime;

pub use {
    config::ThreadManagerConfig,
    native_thread_runtime::{JoinHandle, NativeConfig, NativeThreadRuntime},
    policy::CoreAllocation,
    rayon_runtime::{RayonConfig, RayonRuntime},
    tokio_runtime::{TokioConfig, TokioRuntime},
};

pub const MAX_THREAD_NAME_CHARS: usize = 16;

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
                    warn!("Falling back to default runtime for {name}");
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

    pub fn new(config: &ThreadManagerConfig) -> anyhow::Result<Self> {
        let mut core_allocations = HashMap::<String, Vec<usize>>::new();
        Self::set_process_affinity(config)?;
        let mut manager = ThreadManagerInner::default();
        manager.populate_mappings(config);
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

    /// Explicitly shut down the thread manager and all its runtimes.
    /// This may be useful if you want to ensure that everything has exited cleanly.
    ///
    /// This is reliable for native threads spawned from the runtimes.
    ///
    /// Keep in mind that Tokio may keep workers spun-up even if there is no work for them
    /// due to the way its parking logic works, so this may not be 100% reliable. Also
    /// we do not track Tokio's blocking tasks
    ///
    /// Rayon leakage checks are superficial, and may be easily bypassed by accident through
    /// use of spawn and similar methods that transfer ownership of thread handles to the pool
    ///
    /// It is possible to bypass much of the checks here, their purpose is primarily
    /// to catch silly bugs related to resource leaks
    ///
    /// Just dropping ThreadManager from a normal function is also ok, but dropping it from
    /// async context will cause panic!
    pub fn shutdown(self) -> std::result::Result<(), ShutdownError> {
        let mut inner = match Arc::try_unwrap(self.inner) {
            Ok(inner) => inner,
            Err(inner) => {
                let cnt = Arc::strong_count(&inner).saturating_sub(1);
                error!(
                      "{cnt} strong references to Thread Manager are still active, clean shutdown may not be possible!"
                  );
                return Err(ShutdownError::OtherOwnersExist);
            }
        };

        let mut native_orphans: usize = 0;
        let mut tokio_orphans: usize = 0;
        let mut rayon_orphans: usize = 0;
        // We can not command Tokio runtime to exit immediately, but we can signal shutdown
        // so it does not run any more futures. Normally, you would prefer to ensure nothing is running
        // on the runtime before ThreadManager is terminated.
        for (name, runtime) in inner.tokio_runtimes.drain() {
            let active_cnt = runtime.counters.active_threads_cnt.load(Ordering::SeqCst);
            match active_cnt {
                0 => debug!("Shutting down Tokio runtime {name}"),
                _ => {
                    warn!("Tokio runtime {name} has {active_cnt} active workers during shutdown!");
                    tokio_orphans = tokio_orphans.saturating_add(active_cnt as usize);
                }
            }
            runtime.tokio.shutdown_background();
        }

        // Can not shutdown rayon runtimes as such, but we can ensure noone is holding on to one
        // this is by no means foolproof, as you can call spawn on rayon runtimes and those
        // threads are effectively untraceable. But at least it works for par_iter and such.
        for (name, runtime) in inner.rayon_runtimes.drain() {
            let count = Arc::strong_count(&runtime.inner);
            if Arc::into_inner(runtime.inner).is_none() {
                warn!("Rayon pool {name} has {count} live references during shutdown");
                rayon_orphans = rayon_orphans.saturating_add(1);
            }
        }

        // Similar to other, we can not command native threads to "just exit". However, we can
        // warn if some of them were not joined at thread manager shutdown.
        for (name, runtime) in inner.native_thread_runtimes.drain() {
            let active_cnt = runtime.running_count.load(Ordering::SeqCst);
            match active_cnt {
                0 => debug!("Shutting down Native thread pool {name}"),
                _ => {
                    warn!("Native pool {name} has {active_cnt} active threads during shutdown!");
                    native_orphans = native_orphans.saturating_add(active_cnt);
                }
            }
        }
        if native_orphans > 0 || tokio_orphans > 0 || rayon_orphans > 0 {
            return Err(ShutdownError::OrphanResources {
                native: native_orphans,
                tokio: tokio_orphans,
                rayon: rayon_orphans,
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ShutdownError {
    /// Other owners of the ThreadManager instance exist, can not perform validation
    OtherOwnersExist,

    /// Orphan resources found. This may not be critical, this is mostly a sanity check
    OrphanResources {
        /// Native threads spawned but not joined
        native: usize,
        /// Tokio workers active at exit
        tokio: usize,
        /// Rayon runtimes leaked
        rayon: usize,
    },
}

#[cfg(test)]
mod tests {
    use {
        crate::ThreadManagerConfig,
        std::{io::Read, time::Duration},
    };
    #[cfg(target_os = "linux")]
    use {
        crate::{CoreAllocation, NativeConfig, RayonConfig, ThreadManager},
        std::collections::HashMap,
    };

    #[test]
    fn test_orphans() {
        let manager = ThreadManager::new(&ThreadManagerConfig::default()).unwrap();
        let native = manager.get_native("leaker");
        let tokio = manager.get_tokio("leaker");
        let rayon_rt = manager.get_rayon("leaker").clone();
        tokio.spawn(async {
            // blocking this is REALLY BAD but ok for the test
            std::thread::sleep(Duration::from_millis(200));
        });

        //retain jh so we do not end up dropping in on the floor
        let jh = native
            .spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
            })
            .unwrap();
        if let Err(res) = manager.shutdown() {
            match res {
                crate::ShutdownError::OtherOwnersExist => panic!("Should not have other owners"),
                crate::ShutdownError::OrphanResources {
                    native,
                    tokio,
                    rayon,
                } => {
                    assert_eq!(native, 1, "exactly one native thread should have leaked");
                    assert!(tokio >= 1, "at least one tokio thread should have leaked");
                    assert_eq!(rayon, 1, "exactly one rayon runtime should have leaked");
                }
            }
        } else {
            panic!("Leaked threads not detected!");
        }
        // this is late on purpose
        jh.join().unwrap();
        drop(rayon_rt);
    }

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

        let manager = ThreadManager::new(&conf).unwrap();
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
        manager
            .shutdown()
            .expect("Should not have leaked any resources");
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

        let manager = ThreadManager::new(&conf).unwrap();
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
        manager
            .shutdown()
            .expect("Should not have leaked any resources");
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

        let manager = ThreadManager::new(&conf).unwrap();
        let rayon_runtime = manager.get_rayon("test");

        let _rr = rayon_runtime.rayon_pool.broadcast(|ctx| {
            println!("Rayon thread {} reporting", ctx.index());
            validate_affinity(&[1, 2, 3], "Rayon thread allocation should still be 1-3");
        });
    }
}
