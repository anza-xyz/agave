use {
    agave_thread_manager::*,
    log::info,
    std::{collections::HashMap, time::Duration},
    tokio::sync::oneshot,
};

mod common;
use common::*;

fn make_config_shared(cc: usize) -> ThreadManagerConfig {
    let tokio_cfg_1 = TokioConfig {
        core_allocation: CoreAllocation::DedicatedCoreSet { min: 0, max: cc },
        worker_threads: cc,
        ..Default::default()
    };
    let tokio_cfg_2 = tokio_cfg_1.clone();
    ThreadManagerConfig {
        tokio_configs: HashMap::from([
            ("axum1".into(), tokio_cfg_1),
            ("axum2".into(), tokio_cfg_2),
        ]),
        ..Default::default()
    }
}
fn make_config_dedicated(core_count: usize) -> ThreadManagerConfig {
    let tokio_cfg_1 = TokioConfig {
        core_allocation: CoreAllocation::DedicatedCoreSet {
            min: 0,
            max: core_count / 2,
        },
        worker_threads: core_count / 2,
        ..Default::default()
    };
    let tokio_cfg_2 = TokioConfig {
        core_allocation: CoreAllocation::DedicatedCoreSet {
            min: core_count / 2,
            max: core_count,
        },
        worker_threads: core_count / 2,
        ..Default::default()
    };
    ThreadManagerConfig {
        tokio_configs: HashMap::from([
            ("axum1".into(), tokio_cfg_1),
            ("axum2".into(), tokio_cfg_2),
        ]),
        ..Default::default()
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Regime {
    Shared,
    Dedicated,
    Single,
}
impl Regime {
    const VALUES: [Self; 3] = [Self::Dedicated, Self::Shared, Self::Single];
}

#[derive(Debug, Default, serde::Serialize)]
struct Results {
    latencies_s: Vec<f32>,
    requests_per_second: Vec<f32>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut all_results: HashMap<String, Results> = HashMap::new();
    for regime in Regime::VALUES {
        let mut results = Results::default();
        for core_count in [2, 4, 8, 16] {
            let manager;
            info!("===================");
            info!("Running {core_count} cores under {regime:?}");
            let (tokio1, tokio2) = match regime {
                Regime::Shared => {
                    manager = ThreadManager::new(make_config_shared(core_count)).unwrap();
                    (manager.get_tokio("axum1"), manager.get_tokio("axum2"))
                }
                Regime::Dedicated => {
                    manager = ThreadManager::new(make_config_dedicated(core_count)).unwrap();
                    (manager.get_tokio("axum1"), manager.get_tokio("axum2"))
                }
                Regime::Single => {
                    manager = ThreadManager::new(make_config_shared(core_count)).unwrap();
                    (manager.get_tokio("axum1"), manager.get_tokio("axum2"))
                }
            };

            let workload_runtime = TokioRuntime::new(
                "LoadGenerator".to_owned(),
                TokioConfig {
                    core_allocation: CoreAllocation::DedicatedCoreSet { min: 32, max: 64 },
                    ..Default::default()
                },
            )?;
            let measurement = std::thread::scope(|s| {
                let (tx1, rx1) = oneshot::channel();
                let (tx2, rx2) = oneshot::channel();
                s.spawn(|| {
                    tokio1.start_metrics_sampling(Duration::from_secs(1));
                    tokio1.tokio.block_on(axum_main(8888, tx1));
                });
                let jh = match regime {
                    Regime::Single => s.spawn(|| {
                        rx1.blocking_recv().unwrap();
                        workload_runtime.block_on(workload_main(&[8888, 8888], 3000))
                    }),
                    _ => {
                        s.spawn(|| {
                            tokio2.start_metrics_sampling(Duration::from_secs(1));
                            tokio2.tokio.block_on(axum_main(8889, tx2));
                        });
                        s.spawn(|| {
                            rx1.blocking_recv().unwrap();
                            rx2.blocking_recv().unwrap();
                            workload_runtime.block_on(workload_main(&[8888, 8889], 3000))
                        })
                    }
                };
                jh.join().expect("Some of the threads crashed!")
            })?;
            info!("Results are: {:?}", measurement);
            results.latencies_s.push(measurement.latency_s);
            results
                .requests_per_second
                .push(measurement.requests_per_second);
        }
        all_results.insert(format!("{regime:?}"), results);
        std::thread::sleep(Duration::from_secs(3));
    }

    //print the resulting measurements so they can be e.g. plotted with matplotlib
    println!("{}", serde_json::to_string_pretty(&all_results)?);

    Ok(())
}
