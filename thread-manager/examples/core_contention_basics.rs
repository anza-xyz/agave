use {
    agave_thread_manager::*,
    log::{debug, info},
    std::{
        future::IntoFuture,
        io::{Read, Write},
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

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let experiments = [
        "examples/core_contention_dedicated_set.toml",
        "examples/core_contention_contending_set.toml",
    ];

    for exp in experiments {
        info!("===================");
        info!("Running {exp}");
        let mut conf_file = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        conf_file.push(exp);
        let mut buf = String::new();
        std::fs::File::open(conf_file)?.read_to_string(&mut buf)?;
        let cfg: ThreadManagerConfig = toml::from_str(&buf)?;

        let manager = ThreadManager::new(cfg).unwrap();
        let tokio1 = manager
            .get_tokio("axum1")
            .expect("Expecting runtime named axum1");
        tokio1.start_metrics_sampling(Duration::from_secs(1));
        let tokio2 = manager
            .get_tokio("axum2")
            .expect("Expecting runtime named axum2");
        tokio2.start_metrics_sampling(Duration::from_secs(1));

        let wrk_cores: Vec<_> = (32..64).collect();
        let results = std::thread::scope(|scope| {
            scope.spawn(|| {
                tokio1.tokio.block_on(axum_main(8888));
            });
            scope.spawn(|| {
                tokio2.tokio.block_on(axum_main(8889));
            });
            let join_handle =
                scope.spawn(|| run_wrk(&[8888, 8889], &wrk_cores, wrk_cores.len(), 1000).unwrap());
            join_handle.join().expect("WRK crashed!")
        });
        //print out the results of the bench run
        println!("Results are: {:?}", results);
    }
    Ok(())
}

fn run_wrk(
    ports: &[u16],
    cpus: &[usize],
    threads: usize,
    connections: usize,
) -> anyhow::Result<(Vec<Duration>, Vec<f32>)> {
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
