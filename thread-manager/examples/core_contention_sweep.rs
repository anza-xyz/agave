use {
    agave_thread_manager::*,
    log::{debug, info},
    std::{
        collections::HashMap,
        future::IntoFuture,
        io::Write,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::PathBuf,
        time::Duration,
    },
};

async fn axum_main(port: u16) {
    use axum::{routing::get, Router};
    // basic handler that responds with a static string
    async fn root() -> &'static str {
        tokio::time::sleep(Duration::from_millis(1)).await;
        "Hello, World!"
    }

    // build our application with a route
    let app = Router::new().route("/", get(root));

    // run our app with hyper, listening globally on port 3000
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
            .await
            .unwrap();
    info!("Server on port {port} ready");
    let timeout = tokio::time::timeout(
        Duration::from_secs(11),
        axum::serve(listener, app).into_future(),
    )
    .await;
    match timeout {
        Ok(v) => v.unwrap(),
        Err(_) => {
            info!("Terminating server on port {port}");
        }
    }
}
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
    rps: Vec<f32>,
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
                    (
                        manager
                            .get_tokio("axum1")
                            .expect("Expecting runtime named axum1"),
                        manager
                            .get_tokio("axum2")
                            .expect("Expecting runtime named axum2"),
                    )
                }
                Regime::Dedicated => {
                    manager = ThreadManager::new(make_config_dedicated(core_count)).unwrap();
                    (
                        manager
                            .get_tokio("axum1")
                            .expect("Expecting runtime named axum1"),
                        manager
                            .get_tokio("axum2")
                            .expect("Expecting runtime named axum2"),
                    )
                }
                Regime::Single => {
                    manager = ThreadManager::new(make_config_shared(core_count)).unwrap();
                    (
                        manager
                            .get_tokio("axum1")
                            .expect("Expecting runtime named axum1"),
                        manager
                            .get_tokio("axum2")
                            .expect("Expecting runtime named axum2"),
                    )
                }
            };

            let wrk_cores: Vec<_> = (32..64).collect();
            let measurement = std::thread::scope(|s| {
                s.spawn(|| {
                    tokio1.start_metrics_sampling(Duration::from_secs(1));
                    tokio1.tokio.block_on(axum_main(8888));
                });
                let jh = match regime {
                    Regime::Single => s.spawn(|| {
                        run_wrk(&[8888, 8888], &wrk_cores, wrk_cores.len(), 3000).unwrap()
                    }),
                    _ => {
                        s.spawn(|| {
                            tokio2.start_metrics_sampling(Duration::from_secs(1));
                            tokio2.tokio.block_on(axum_main(8889));
                        });
                        s.spawn(|| {
                            run_wrk(&[8888, 8889], &wrk_cores, wrk_cores.len(), 3000).unwrap()
                        })
                    }
                };
                jh.join().expect("WRK crashed!")
            });
            info!("Results are: {:?}", measurement);
            results.latencies_s.push(
                measurement.0.iter().map(|a| a.as_secs_f32()).sum::<f32>()
                    / measurement.0.len() as f32,
            );
            results.rps.push(measurement.1.iter().sum());
        }
        all_results.insert(format!("{regime:?}"), results);
        std::thread::sleep(Duration::from_secs(3));
    }

    //print the resulting measurements so they can be e.g. plotted with matplotlib
    println!("{}", serde_json::to_string_pretty(&all_results)?);

    Ok(())
}

fn run_wrk(
    ports: &[u16],
    cpus: &[usize],
    threads: usize,
    connections: usize,
) -> anyhow::Result<(Vec<Duration>, Vec<f32>)> {
    //Sleep a bit to let axum start
    std::thread::sleep(Duration::from_millis(500));

    let mut script = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    script.push("examples/report.lua");
    let cpus: Vec<String> = cpus.iter().map(|c| c.to_string()).collect();
    let cpus = cpus.join(",");

    let mut children: Vec<_> = ports
        .iter()
        .map(|p| {
            std::process::Command::new("taskset")
                .arg("-c")
                .arg(&cpus)
                .arg("wrk")
                .arg(format!("http://localhost:{}", p))
                .arg("-d10")
                .arg(format!("-s{}", script.to_str().unwrap()))
                .arg(format!("-t{threads}"))
                .arg(format!("-c{connections}"))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();

    use std::str;
    let outs = children.drain(..).map(|c| c.wait_with_output().unwrap());
    let mut all_latencies = vec![];
    let mut all_rps = vec![];
    for (out, port) in outs.zip(ports.iter()) {
        debug!("=========================");
        std::io::stdout().write_all(&out.stderr)?;
        let res = str::from_utf8(&out.stdout)?;
        let mut res = res.lines().last().unwrap().split(' ');

        let latency_us: u64 = res.next().unwrap().parse()?;
        let latency = Duration::from_micros(latency_us);

        let requests: usize = res.next().unwrap().parse()?;
        let rps = requests as f32 / 10.0;
        debug!("WRK results for port {port}: {latency:?} {rps}");
        all_latencies.push(Duration::from_micros(latency_us));
        all_rps.push(rps);
    }
    Ok((all_latencies, all_rps))
}
