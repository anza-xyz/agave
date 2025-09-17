use {
    axum::{routing::get, Router},
    hyper::{Body, Request},
    log::info,
    std::{
        future::IntoFuture,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    },
    tokio::{net::TcpStream, sync::oneshot::Sender, task::JoinSet, time::timeout},
    tower::ServiceExt,
};
// 10 seconds is just enough to get a reasonably accurate measurement under our crude methodology
const TEST_SECONDS: u64 = 10;

// A simple web server that puts load on tokio to measure thread contention impacts
pub async fn axum_main(port: u16, ready: Sender<()>) {
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
    ready.send(()).unwrap();
    let timeout = tokio::time::timeout(
        Duration::from_secs(TEST_SECONDS + 1),
        axum::serve(listener, app).into_future(),
    )
    .await;
    match timeout {
        Ok(v) => {
            v.unwrap();
        }
        Err(_) => {
            info!("Terminating server on port {port}");
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct Stats {
    pub latency_s: f32,
    pub requests_per_second: f32,
}

// Generates a bunch of HTTP load on the ports provided. Will spawn `tasks` worth
// of connections for each of the ports given.
pub async fn workload_main(ports: &[u16], tasks: usize) -> anyhow::Result<Stats> {
    struct ControlBlock {
        start_time: std::time::Instant,
        requests: AtomicUsize,
        cumulative_latency_us: AtomicUsize,
    }

    let control_block = Arc::new(ControlBlock {
        start_time: std::time::Instant::now(),
        requests: AtomicUsize::new(0),
        cumulative_latency_us: AtomicUsize::new(0),
    });

    async fn connection(port: u16, control_block: Arc<ControlBlock>) -> anyhow::Result<()> {
        let sa = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let stream = TcpStream::connect(sa).await?;

        let (mut request_sender, connection) = hyper::client::conn::handshake(stream).await?;
        // spawn a task to poll the connection and drive the HTTP state
        tokio::spawn(async move {
            // Technically, this can error but this only happens when server is killed
            let _ = connection.await;
        });

        let path = "/";
        while control_block.start_time.elapsed() < Duration::from_secs(TEST_SECONDS) {
            let req = Request::builder()
                .uri(path)
                .method("GET")
                .body(Body::from(""))?;
            let start = Instant::now();
            let res = timeout(Duration::from_millis(100), request_sender.send_request(req)).await;
            let res = match res {
                Ok(res) => res?,
                Err(_) => {
                    anyhow::bail!("Timeout on request!")
                }
            };
            let _ = res.body();
            if res.status() != 200 {
                anyhow::bail!("Got error from server");
            }

            control_block
                .cumulative_latency_us
                .fetch_add(start.elapsed().as_micros() as usize, Ordering::Relaxed);
            control_block.requests.fetch_add(1, Ordering::Relaxed);
            // To send via the same connection again, it may not work as it may not be ready,
            // so we have to wait until the request_sender becomes ready.
            request_sender.ready().await?;
        }
        Ok(())
    }

    let mut join_set = JoinSet::new();
    for port in ports {
        info!("Starting load generation on port {port}");
        for _t in 0..tasks {
            join_set.spawn(connection(*port, control_block.clone()));
        }
    }

    while let Some(jr) = join_set.join_next().await {
        jr??;
    }

    let requests = control_block.requests.load(Ordering::Relaxed);
    let latency_accumulator_us = control_block.cumulative_latency_us.load(Ordering::Relaxed);
    Ok(Stats {
        requests_per_second: requests as f32 / TEST_SECONDS as f32,
        #[allow(clippy::arithmetic_side_effects)]
        latency_s: (latency_accumulator_us / requests) as f32 / 1e6,
    })
}
