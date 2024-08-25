use {
    crate::metrics::Metrics,
    axum::{http::StatusCode, routing::get, Router},
    log::info,
    std::{
        net::SocketAddr,
        sync::Arc,
        thread::{self, JoinHandle},
    },
    tokio::{net::TcpListener, runtime::Runtime},
};

pub struct PrometheusMetricsService {
    // thread handler
    thread_hdl: JoinHandle<()>,
}

impl PrometheusMetricsService {
    pub fn new(prometheus_addr: SocketAddr, metrics: Option<Arc<Metrics>>) -> Self {
        info!("prometheus bound to {:?}", prometheus_addr);

        let thread_hdl = thread::spawn(move || {
            let runtime = Runtime::new().unwrap();
            runtime.block_on(async move {
                let app = Router::new()
                    .route("/ping", get(|| async { "pong" }))
                    .route(
                        "/metrics",
                        get(move || async move {
                            match metrics {
                                None => Err((
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "Internal Server Error".to_string(),
                                )),
                                Some(metrics) => match metrics.dump() {
                                    Ok(message) => Ok(message),
                                    Err(_e) => Err((
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        "Internal Server Error".to_string(),
                                    )),
                                },
                            }
                        }),
                    );

                let listener = TcpListener::bind(prometheus_addr).await.unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });

        Self { thread_hdl }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
