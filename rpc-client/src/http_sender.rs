//! Nonblocking [`RpcSender`] over HTTP.

use {
    crate::rpc_sender::*,
    async_trait::async_trait,
    log::*,
    reqwest::{
        self, StatusCode,
        header::{self, CONTENT_TYPE, RETRY_AFTER},
    },
    solana_rpc_client_api::{
        client_error::Result,
        custom_error,
        error_object::RpcErrorObject,
        request::{RpcError, RpcRequest, RpcResponseErrorData},
        response::RpcSimulateTransactionResult,
    },
    std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    },
    tokio::time::sleep,
};

pub struct HttpSender {
    client: Arc<reqwest_middleware::ClientWithMiddleware>,
    url: String,
    request_id: AtomicU64,
    stats: RwLock<RpcTransportStats>,
    max_rate_limit_retries: usize,
}

/// Nonblocking [`RpcSender`] over HTTP.
impl HttpSender {
    /// Create an HTTP RPC sender.
    ///
    /// The URL is an HTTP URL, usually for port 8899, as in
    /// "http://localhost:8899". The sender has a default timeout of 30 seconds.
    pub fn new<U: ToString>(url: U) -> Self {
        Self::new_with_timeout(url, Duration::from_secs(30))
    }

    /// Create an HTTP RPC sender.
    ///
    /// The URL is an HTTP URL, usually for port 8899.
    /// The timeout applies to each HTTP request, excluding waits between HTTP 429
    /// retries. Use [`Self::with_max_rate_limit_retries`] to limit those retries.
    pub fn new_with_timeout<U: ToString>(url: U, timeout: Duration) -> Self {
        Self::new_with_client(
            url,
            reqwest::Client::builder()
                .default_headers(Self::default_headers())
                .timeout(timeout)
                .pool_idle_timeout(timeout)
                .build()
                .expect("build rpc client"),
        )
    }

    /// Create an HTTP RPC sender.
    ///
    /// Most flexible way to create a sender. Pass a created `reqwest::Client`.
    pub fn new_with_client<U: ToString>(url: U, client: reqwest::Client) -> Self {
        Self {
            client: Arc::new(reqwest_middleware::ClientBuilder::new(client).build()),
            url: url.to_string(),
            request_id: AtomicU64::new(0),
            stats: RwLock::new(RpcTransportStats::default()),
            max_rate_limit_retries: 5,
        }
    }

    /// Create an HTTP RPC sender.
    ///
    /// Most flexible way to create a sender with middleware. Pass a created `reqwest_middleware::ClientWithMiddleware`.
    pub fn new_with_client_with_middleware<U: ToString>(
        url: U,
        client: reqwest_middleware::ClientWithMiddleware,
    ) -> Self {
        Self {
            client: Arc::new(client),
            url: url.to_string(),
            request_id: AtomicU64::new(0),
            stats: RwLock::new(RpcTransportStats::default()),
            max_rate_limit_retries: 5,
        }
    }

    /// Set the maximum number of retries after an HTTP 429 response per RPC call.
    ///
    /// The default is five retries, for up to six HTTP requests. Set this to zero
    /// to return the first HTTP 429 error without waiting or retrying, allowing
    /// the caller to apply its own retry policy. This does not affect retries
    /// performed by middleware supplied to [`Self::new_with_client_with_middleware`].
    /// Rate-limit retries use the `Retry-After` delay when supported, or a default
    /// delay of 500 milliseconds.
    pub fn with_max_rate_limit_retries(mut self, max_rate_limit_retries: usize) -> Self {
        self.max_rate_limit_retries = max_rate_limit_retries;
        self
    }

    /// Create default headers used by HTTP Sender.
    pub fn default_headers() -> header::HeaderMap {
        let mut default_headers = header::HeaderMap::new();
        default_headers.append(
            header::HeaderName::from_static("solana-client"),
            header::HeaderValue::from_str(
                format!("rust/{}", solana_version::Version::default()).as_str(),
            )
            .unwrap(),
        );
        default_headers
    }
}

struct StatsUpdater<'a> {
    stats: &'a RwLock<RpcTransportStats>,
    request_start_time: Instant,
    rate_limited_time: Duration,
}

impl<'a> StatsUpdater<'a> {
    fn new(stats: &'a RwLock<RpcTransportStats>) -> Self {
        Self {
            stats,
            request_start_time: Instant::now(),
            rate_limited_time: Duration::default(),
        }
    }

    fn add_rate_limited_time(&mut self, duration: Duration) {
        self.rate_limited_time += duration;
    }
}

impl Drop for StatsUpdater<'_> {
    fn drop(&mut self) {
        let mut stats = self.stats.write().unwrap();
        stats.request_count += 1;
        stats.elapsed_time += Instant::now().duration_since(self.request_start_time);
        stats.rate_limited_time += self.rate_limited_time;
    }
}

#[async_trait]
impl RpcSender for HttpSender {
    fn get_transport_stats(&self) -> RpcTransportStats {
        self.stats.read().unwrap().clone()
    }

    async fn send(
        &self,
        request: RpcRequest,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut stats_updater = StatsUpdater::new(&self.stats);

        let request_id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let request_json = request.build_request_json(request_id, params).to_string();

        let mut too_many_requests_retries = self.max_rate_limit_retries;
        loop {
            let response = {
                let client = self.client.clone();
                let request_json = request_json.clone();
                client
                    .post(&self.url)
                    .header(CONTENT_TYPE, "application/json")
                    .body(request_json)
                    .send()
                    .await
            }?;

            if !response.status().is_success() {
                if response.status() == StatusCode::TOO_MANY_REQUESTS
                    && too_many_requests_retries > 0
                {
                    let mut duration = Duration::from_millis(500);
                    if let Some(retry_after) = response.headers().get(RETRY_AFTER)
                        && let Ok(retry_after) = retry_after.to_str()
                        && let Ok(retry_after) = retry_after.parse::<u64>()
                        && retry_after < 120
                    {
                        duration = Duration::from_secs(retry_after);
                    }

                    too_many_requests_retries -= 1;
                    debug!(
                        "Too many requests: server responded with {response:?}, \
                         {too_many_requests_retries} retries left, pausing for {duration:?}"
                    );

                    sleep(duration).await;
                    stats_updater.add_rate_limited_time(duration);
                    continue;
                }
                return Err(response.error_for_status().unwrap_err().into());
            }

            let mut json = response.json::<serde_json::Value>().await?;
            if !json.is_object() {
                return Err(RpcError::RpcRequestError(format!(
                    "RPC response is not a JSON object: {json}",
                ))
                .into());
            }
            if json["error"].is_object() {
                return match serde_json::from_value::<RpcErrorObject>(json["error"].clone()) {
                    Ok(rpc_error_object) => {
                        let data = match rpc_error_object.code {
                                    custom_error::JSON_RPC_SERVER_ERROR_SEND_TRANSACTION_PREFLIGHT_FAILURE => {
                                        match serde_json::from_value::<RpcSimulateTransactionResult>(json["error"]["data"].clone()) {
                                            Ok(data) => RpcResponseErrorData::SendTransactionPreflightFailure(data),
                                            Err(err) => {
                                                debug!("Failed to deserialize RpcSimulateTransactionResult: {err:?}");
                                                RpcResponseErrorData::Empty
                                            }
                                        }
                                    },
                                    custom_error::JSON_RPC_SERVER_ERROR_NODE_UNHEALTHY => {
                                        match serde_json::from_value::<custom_error::NodeUnhealthyErrorData>(json["error"]["data"].clone()) {
                                            Ok(custom_error::NodeUnhealthyErrorData {num_slots_behind}) => RpcResponseErrorData::NodeUnhealthy {num_slots_behind},
                                            Err(_err) => {
                                                RpcResponseErrorData::Empty
                                            }
                                        }
                                    },
                                    _ => RpcResponseErrorData::Empty
                                };

                        Err(RpcError::RpcResponseError {
                            code: rpc_error_object.code,
                            message: rpc_error_object.message,
                            data,
                        }
                        .into())
                    }
                    Err(err) => Err(RpcError::RpcRequestError(format!(
                        "Failed to deserialize RPC error response: {} [{}]",
                        serde_json::to_string(&json["error"]).unwrap(),
                        err
                    ))
                    .into()),
                };
            }
            return Ok(json["result"].take());
        }
    }

    fn url(&self) -> String {
        self.url.clone()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_rpc_client_api::client_error::ErrorKind,
        tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
            task::JoinHandle,
            time::timeout,
        },
    };

    struct TestServer {
        url: String,
        request_count: Arc<AtomicU64>,
        task: JoinHandle<()>,
    }

    impl TestServer {
        async fn new(statuses: Vec<StatusCode>, retry_after: Option<&str>) -> Self {
            assert!(!statuses.is_empty());
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let request_count = Arc::new(AtomicU64::new(0));
            let server_request_count = request_count.clone();
            let retry_after = retry_after
                .map(|value| format!("Retry-After: {value}\r\n"))
                .unwrap_or_default();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let count = stream.read(&mut buffer).await.unwrap();
                        assert_ne!(count, 0);
                        request.extend_from_slice(&buffer[..count]);
                        if let Some(headers_end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            let headers = std::str::from_utf8(&request[..headers_end]).unwrap();
                            let content_length = headers
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                                .unwrap_or_default();
                            if request.len() >= headers_end + 4 + content_length {
                                break;
                            }
                        }
                    }
                    let index = server_request_count.fetch_add(1, Ordering::Relaxed) as usize;
                    let status = statuses[index.min(statuses.len() - 1)];
                    let body = r#"{"jsonrpc":"2.0","id":0,"result":42}"#;
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: \
                         {}\r\nConnection: close\r\n{retry_after}\r\n{body}",
                        body.len(),
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            Self {
                url,
                request_count,
                task,
            }
        }

        fn request_count(&self) -> u64 {
            self.request_count.load(Ordering::Relaxed)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn assert_http_error(error: solana_rpc_client_api::client_error::Error, status: StatusCode) {
        let ErrorKind::Reqwest(error) = error.kind() else {
            panic!("expected reqwest error, got {error:?}");
        };
        assert_eq!(error.status(), Some(status));
        assert!(!error.is_timeout());
    }

    #[tokio::test]
    async fn test_default_rate_limit_retries() {
        let server = TestServer::new(vec![StatusCode::TOO_MANY_REQUESTS], Some("0")).await;
        let client = reqwest::Client::new();
        let middleware_client = reqwest_middleware::ClientBuilder::new(client.clone()).build();
        let senders = [
            ("new", HttpSender::new(&server.url)),
            (
                "new_with_timeout",
                HttpSender::new_with_timeout(&server.url, Duration::from_secs(1)),
            ),
            (
                "new_with_client",
                HttpSender::new_with_client(&server.url, client),
            ),
            (
                "new_with_client_with_middleware",
                HttpSender::new_with_client_with_middleware(&server.url, middleware_client),
            ),
        ];
        for (constructor, sender) in senders {
            let previous_request_count = server.request_count();
            let result = sender
                .send(RpcRequest::GetVersion, serde_json::Value::Null)
                .await;
            assert_http_error(result.unwrap_err(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(
                server.request_count() - previous_request_count,
                6,
                "{constructor} must make the initial request and five retries",
            );
            assert_eq!(sender.get_transport_stats().request_count, 1);
        }
    }

    #[tokio::test]
    async fn test_disable_rate_limit_retries() {
        let server = TestServer::new(vec![StatusCode::TOO_MANY_REQUESTS], Some("10")).await;
        let sender = HttpSender::new_with_timeout(&server.url, Duration::from_millis(50))
            .with_max_rate_limit_retries(0);
        let result = timeout(
            Duration::from_secs(1),
            sender.send(RpcRequest::GetVersion, serde_json::Value::Null),
        )
        .await
        .expect("disabling retries must not wait for Retry-After");
        assert_http_error(result.unwrap_err(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(server.request_count(), 1);
        let stats = sender.get_transport_stats();
        assert_eq!(stats.request_count, 1);
        assert_eq!(stats.rate_limited_time, Duration::ZERO);
    }

    #[tokio::test]
    async fn test_rate_limit_retry_count() {
        let server = TestServer::new(vec![StatusCode::TOO_MANY_REQUESTS], Some("0")).await;
        let sender = HttpSender::new(&server.url).with_max_rate_limit_retries(2);
        let result = sender
            .send(RpcRequest::GetVersion, serde_json::Value::Null)
            .await;
        assert_http_error(result.unwrap_err(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(server.request_count(), 3);
        assert_eq!(sender.get_transport_stats().request_count, 1);
    }

    #[tokio::test]
    async fn test_rate_limit_retry_recovers_and_resets_per_call() {
        let server = TestServer::new(
            vec![
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::OK,
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::OK,
            ],
            Some("0"),
        )
        .await;
        let sender = HttpSender::new(&server.url).with_max_rate_limit_retries(1);
        for _ in 0..2 {
            assert_eq!(
                sender
                    .send(RpcRequest::GetVersion, serde_json::Value::Null)
                    .await
                    .unwrap(),
                serde_json::json!(42),
            );
        }
        assert_eq!(server.request_count(), 4);
        assert_eq!(sender.get_transport_stats().request_count, 2);
    }

    #[tokio::test]
    async fn test_rate_limit_retries_preserve_retry_after_and_per_attempt_timeout() {
        let server = TestServer::new(
            vec![StatusCode::TOO_MANY_REQUESTS, StatusCode::OK],
            Some("1"),
        )
        .await;
        let sender = HttpSender::new_with_timeout(&server.url, Duration::from_millis(50))
            .with_max_rate_limit_retries(1);
        let start = Instant::now();
        let result = sender
            .send(RpcRequest::GetVersion, serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(42));
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert_eq!(server.request_count(), 2);
        assert_eq!(
            sender.get_transport_stats().rate_limited_time,
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn test_rate_limit_retry_setting_does_not_retry_other_http_errors() {
        let server = TestServer::new(vec![StatusCode::SERVICE_UNAVAILABLE], Some("10")).await;
        let sender = HttpSender::new(&server.url).with_max_rate_limit_retries(2);
        let result = sender
            .send(RpcRequest::GetVersion, serde_json::Value::Null)
            .await;
        assert_http_error(result.unwrap_err(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(server.request_count(), 1);
        assert_eq!(
            sender.get_transport_stats().rate_limited_time,
            Duration::ZERO
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_sender_on_tokio_multi_thread() {
        let http_sender = HttpSender::new("http://localhost:1234".to_string());
        let _ = http_sender
            .send(RpcRequest::GetVersion, serde_json::Value::Null)
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_sender_on_tokio_current_thread() {
        let http_sender = HttpSender::new("http://localhost:1234".to_string());
        let _ = http_sender
            .send(RpcRequest::GetVersion, serde_json::Value::Null)
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_sender_returns_error_for_non_object_json_response() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn a server that returns a JSON string instead of a JSON object,
        // simulating a misbehaving RPC proxy or load balancer.
        tokio::task::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            stream.readable().await.unwrap();
            let _ = stream.try_read(&mut buf);
            let body = r#""Service Unavailable""#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body,
            );
            stream.writable().await.unwrap();
            stream.try_write(response.as_bytes()).unwrap();
        });

        let http_sender = HttpSender::new(format!("http://{addr}"));
        let result = http_sender
            .send(RpcRequest::GetVersion, serde_json::Value::Null)
            .await;

        let err = result.unwrap_err();
        let err_string = err.to_string();
        assert!(
            err_string.contains("not a JSON object"),
            "Expected 'not a JSON object' error, got: {err_string}"
        );
    }
}
