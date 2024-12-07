use std::{
    future::IntoFuture,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
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
            println!("Terminating server on port {port}");
        }
    }
}
use agave_thread_manager::*;

fn main() -> anyhow::Result<()> {
    let experiments = [
        "examples/core_contention_dedicated_set.json",
        "examples/core_contention_contending_set.json",
    ];

    for exp in experiments {
        println!("===================");
        println!("Running {exp}");
        let mut conffile = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        conffile.push(exp);
        let conffile = std::fs::File::open(conffile)?;
        let cfg: RuntimeManagerConfig = serde_json::from_reader(conffile)?;
        //println!("Loaded config {}", serde_json::to_string_pretty(&cfg)?);

        let rtm = RuntimeManager::new(cfg).unwrap();
        let tok1 = rtm
            .get_tokio("axum1")
            .expect("Expecting runtime named axum1");
        let tok2 = rtm
            .get_tokio("axum2")
            .expect("Expecting runtime named axum2");

        let wrk_cores: Vec<_> = (32..64).collect();
        let results = std::thread::scope(|s| {
            s.spawn(|| {
                tok1.start(axum_main(8888));
            });
            s.spawn(|| {
                tok2.start(axum_main(8889));
            });
            let jh = s.spawn(|| run_wrk(&[8888, 8889], &wrk_cores, wrk_cores.len(), 1000).unwrap());
            jh.join().expect("WRK crashed!")
        });
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
        println!("=========================");
        std::io::stdout().write_all(&out.stderr)?;
        let res = str::from_utf8(&out.stdout)?;
        let mut res = res.lines().last().unwrap().split(' ');

        let latency_us: u64 = res.next().unwrap().parse()?;
        let latency = Duration::from_micros(latency_us);

        let requests: usize = res.next().unwrap().parse()?;
        let rps = requests as f32 / 10.0;
        println!("WRK results for port {port}: {latency:?} {rps}");
        all_latencies.push(Duration::from_micros(latency_us));
        all_rps.push(rps);
    }
    Ok((all_latencies, all_rps))
}
