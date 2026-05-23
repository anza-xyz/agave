use {
    crate::{
        HEADER_LENGTH, IP_ECHO_SERVER_RESPONSE_LENGTH,
        ip_echo_server::{IpEchoServerMessage, IpEchoServerResponse},
    },
    bytes::{BufMut, BytesMut},
    std::{
        net::{IpAddr, SocketAddr},
        time::Duration,
    },
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpSocket,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum IpEchoClientError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Timeout(#[from] tokio::time::error::Elapsed),
    #[error(transparent)]
    BincodeError(#[from] bincode::Error),
    #[error("{0}")]
    InvalidResponse(String),
}

/// Applies to all operations with the echo server.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);

/// Make a request to the echo server, binding the client socket to the provided IP.
pub(crate) async fn ip_echo_server_request_with_binding(
    ip_echo_server_addr: SocketAddr,
    msg: IpEchoServerMessage,
    bind_address: IpAddr,
) -> Result<IpEchoServerResponse, IpEchoClientError> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::new(bind_address, 0))?;

    let response =
        tokio::time::timeout(TIMEOUT, make_request(socket, ip_echo_server_addr, msg)).await??;
    parse_response(response, ip_echo_server_addr)
}

/// Make a request to the echo server, client socket will be bound by the OS.
pub(crate) async fn ip_echo_server_request(
    ip_echo_server_addr: SocketAddr,
    msg: IpEchoServerMessage,
) -> Result<IpEchoServerResponse, IpEchoClientError> {
    let socket = TcpSocket::new_v4()?;
    let response =
        tokio::time::timeout(TIMEOUT, make_request(socket, ip_echo_server_addr, msg)).await??;
    parse_response(response, ip_echo_server_addr)
}

/// Makes the request to the specified server and returns reply as Bytes.
async fn make_request(
    socket: TcpSocket,
    ip_echo_server_addr: SocketAddr,
    msg: IpEchoServerMessage,
) -> Result<BytesMut, IpEchoClientError> {
    let mut stream = socket.connect(ip_echo_server_addr).await?;
    // Start with HEADER_LENGTH null bytes to avoid looking like an HTTP GET/POST request
    let mut bytes = BytesMut::with_capacity(IP_ECHO_SERVER_RESPONSE_LENGTH);
    bytes.extend_from_slice(&[0u8; HEADER_LENGTH]);
    bytes.extend_from_slice(&bincode::serialize(&msg)?);

    // End with '\n' to make this request look HTTP-ish and tickle an error response back
    // from an HTTP server
    bytes.put_u8(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;

    bytes.clear();
    let _n = stream.read_buf(&mut bytes).await?;
    stream.shutdown().await?;

    Ok(bytes)
}

fn parse_response(
    response: BytesMut,
    ip_echo_server_addr: SocketAddr,
) -> Result<IpEchoServerResponse, IpEchoClientError> {
    // It's common for users to accidentally confuse the validator's gossip port and JSON
    // RPC port.  Attempt to detect when this occurs by looking for the standard HTTP
    // response header and provide the user with a helpful error message
    if response.len() < HEADER_LENGTH {
        return Err(IpEchoClientError::InvalidResponse(format!(
            "Response too short, received {} bytes",
            response.len()
        )));
    }

    let (response_header, body) =
        response
            .split_first_chunk::<HEADER_LENGTH>()
            .ok_or_else(|| {
                IpEchoClientError::InvalidResponse(format!(
                    "Not enough data in the response from {ip_echo_server_addr}!"
                ))
            })?;
    let payload = match response_header {
        [0, 0, 0, 0] => bincode::deserialize(body)?,
        [b'H', b'T', b'T', b'P'] => {
            let http_response = std::str::from_utf8(body);
            match http_response {
                Ok(r) => {
                    return Err(IpEchoClientError::InvalidResponse(format!(
                        "Invalid gossip entrypoint. {ip_echo_server_addr} looks to be an HTTP \
                         port replying with {r}"
                    )));
                }
                Err(_) => {
                    return Err(IpEchoClientError::InvalidResponse(format!(
                        "Invalid gossip entrypoint. {ip_echo_server_addr} looks to be an HTTP \
                         port."
                    )));
                }
            }
        }
        _ => {
            return Err(IpEchoClientError::InvalidResponse(format!(
                "Invalid gossip entrypoint. {ip_echo_server_addr} provided unexpected header \
                 bytes {response_header:?} "
            )));
        }
    };
    Ok(payload)
}
