use {
    base64::{Engine as _, engine::general_purpose::STANDARD},
    http::Uri,
    hyper_util::rt::TokioIo,
    std::{
        future::Future,
        io,
        pin::Pin,
        task::{Context, Poll},
    },
    tokio::io::{AsyncReadExt, AsyncWriteExt},
    tonic::codegen::Service,
};

const CONNECT_RESPONSE_MAX_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug)]
pub struct ProxyConnector {
    proxy_uri: Uri,
    proxy_authorization: Option<String>,
}

impl ProxyConnector {
    pub fn new(proxy_uri: Uri) -> Result<Self, String> {
        if proxy_uri.host().is_none() {
            return Err("proxy URI missing host".to_string());
        }
        Ok(Self {
            proxy_authorization: proxy_authorization(&proxy_uri),
            proxy_uri,
        })
    }
}

impl Service<Uri> for ProxyConnector {
    type Response = TokioIo<tokio::net::TcpStream>;
    type Error = io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let proxy_uri = self.proxy_uri.clone();
        let proxy_authorization = self.proxy_authorization.clone();

        Box::pin(async move {
            let mut stream = connect_proxy(&proxy_uri).await?;
            let connect_target = connect_target(&dst)?;

            write_connect_request(&mut stream, &connect_target, proxy_authorization.as_deref())
                .await?;
            ensure_connect_response_success(&mut stream).await?;
            Ok(TokioIo::new(stream))
        })
    }
}

fn connect_target(dst: &Uri) -> Result<String, io::Error> {
    let host = dst
        .host()
        .ok_or_else(|| io::Error::other("destination URI missing host"))?;
    let port = dst
        .port_u16()
        .unwrap_or(if dst.scheme_str() == Some("http") {
            80
        } else {
            443
        });
    Ok(format!("{host}:{port}"))
}

fn proxy_authorization(proxy_uri: &Uri) -> Option<String> {
    let authority = proxy_uri.authority()?.as_str();
    let (userinfo, _) = authority.rsplit_once('@')?;
    let (user, pass) = userinfo.split_once(':')?;
    Some(format!(
        "Basic {}",
        STANDARD.encode(format!("{user}:{pass}"))
    ))
}

async fn write_connect_request(
    stream: &mut tokio::net::TcpStream,
    connect_target: &str,
    proxy_authorization: Option<&str>,
) -> io::Result<()> {
    let mut request = format!("CONNECT {connect_target} HTTP/1.1\r\nHost: {connect_target}\r\n");
    if let Some(proxy_authorization) = proxy_authorization {
        request.push_str(&format!("Proxy-Authorization: {proxy_authorization}\r\n"));
    }
    request.push_str("\r\n");

    stream.write_all(request.as_bytes()).await?;
    stream.flush().await
}

async fn ensure_connect_response_success(stream: &mut tokio::net::TcpStream) -> io::Result<()> {
    let mut response = Vec::with_capacity(512);
    loop {
        if response.len() >= CONNECT_RESPONSE_MAX_BYTES {
            return Err(io::Error::other("proxy CONNECT response too large"));
        }

        let mut buf = [0_u8; 512];
        let read = stream.read(&mut buf).await?;
        if read == 0 {
            return Err(io::Error::other(
                "unexpected EOF while reading proxy CONNECT response",
            ));
        }
        response.extend_from_slice(&buf[..read]);

        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            if response.starts_with(b"HTTP/1.1 200") || response.starts_with(b"HTTP/1.0 200") {
                return Ok(());
            }
            let summary_end = response
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(response.len())
                .min(80);
            let summary = String::from_utf8_lossy(&response[..summary_end]);
            return Err(io::Error::other(format!(
                "unsuccessful proxy CONNECT response: {summary}"
            )));
        }
    }
}

async fn connect_proxy(proxy_uri: &Uri) -> io::Result<tokio::net::TcpStream> {
    let host = proxy_uri
        .host()
        .ok_or_else(|| io::Error::other("proxy URI missing host"))?;
    let port = proxy_uri
        .port_u16()
        .unwrap_or(if proxy_uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });

    let stream = tokio::net::TcpStream::connect((host, port)).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use {super::*, http::Uri};

    #[test]
    fn test_proxy_authorization_from_uri() {
        let uri = Uri::from_static("http://bob:secret@proxy.example:8080");
        assert_eq!(
            proxy_authorization(&uri),
            Some("Basic Ym9iOnNlY3JldA==".to_string())
        );
    }

    #[test]
    fn test_proxy_authorization_without_user_info() {
        let uri = Uri::from_static("http://proxy.example:8080");
        assert_eq!(proxy_authorization(&uri), None);
    }
}
