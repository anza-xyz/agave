//! One port serving the embedded single-page app over HTTP and the state feed
//! over a websocket at `/websocket`. Routing peeks at the request head, which
//! leaves the bytes in the socket for soketto's handshake.

use {
    crate::{
        collect::EpochInfo,
        history::SlotHistory,
        proto::{MAX_MESSAGE, Message, Publisher, Request, encode_with_id},
        validator_info::ValidatorInfoCache,
    },
    soketto::handshake::{Server, server},
    solana_clock::Slot,
    std::{
        io,
        net::IpAddr,
        sync::{Arc, RwLock},
    },
    thiserror::Error,
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        pin, select,
        sync::{
            Semaphore,
            broadcast::error::{RecvError, TryRecvError},
        },
        time::{Duration, timeout},
    },
    tokio_util::compat::TokioAsyncReadCompatExt,
};

const WEBSOCKET_PATH: &str = "/websocket";

/// Cap on the request head we are willing to buffer before giving up.
const MAX_REQUEST_HEAD: usize = 8192;

/// How long a client has to send its request head before being dropped.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Clients only send small control frames. Anything larger is either a client
/// bug or an attempt to make the server allocate, so the connection is closed.
const MAX_CLIENT_MESSAGE: usize = 4096;

/// How long one send may block before the client is treated as gone. A viewer
/// that stops reading holds a connection slot until TCP notices, which takes
/// minutes. Not an inbound-idle timeout: a healthy viewer never sends.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Websocket clients served at once. They are the long-lived resource;
/// generous enough that several tabs never notice it.
const MAX_WEBSOCKET_CLIENTS: usize = 64;

/// Connections served at once, websockets included. Every request in flight
/// holds a copy of what it answers with, so unbounded a request flood is a
/// memory amplifier in a process whose death takes the validator too. A
/// ceiling, not a throttle.
const MAX_CONNECTIONS: usize = 256;

/// The caps a connection has to pass. Named so the two semaphores cannot be
/// transposed.
#[derive(Clone)]
struct Limits {
    connections: Arc<Semaphore>,
    websockets: Arc<Semaphore>,
}

impl Limits {
    fn new() -> Self {
        Self {
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            websockets: Arc::new(Semaphore::new(MAX_WEBSOCKET_CLIENTS)),
        }
    }
}

/// Served when the crate was built without `frontend/dist` present.
const MISSING_FRONTEND: &str = include_str!("missing_frontend.html");

mod assets {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

#[derive(Debug, Error)]
enum ConnectionError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("handshake error: {0}")]
    Handshake(#[from] soketto::handshake::Error),
    #[error("connection error: {0}")]
    Connection(#[from] soketto::connection::Error),
    #[error("client fell too far behind and was disconnected")]
    Lagged,
    #[error("client sent an oversized message of {0} bytes")]
    Oversized(usize),
}

pub async fn serve(
    listener: TcpListener,
    publisher: Arc<Publisher>,
    history: Arc<RwLock<SlotHistory>>,
    info: Arc<RwLock<ValidatorInfoCache>>,
    epochs: Arc<RwLock<Vec<EpochInfo>>>,
    allowed_hosts: Arc<[String]>,
) {
    let limits = Limits::new();
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                log::warn!("dashboard: accept failed: {err}");
                continue;
            }
        };
        let publisher = publisher.clone();
        let history = history.clone();
        let info = info.clone();
        let epochs = epochs.clone();
        let limits = limits.clone();
        let allowed_hosts = allowed_hosts.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(
                socket,
                publisher,
                history,
                info,
                epochs,
                limits,
                &allowed_hosts,
            )
            .await
            {
                log::debug!("dashboard: connection from {peer} ended: {err}");
            }
        });
    }
}

async fn handle(
    mut socket: TcpStream,
    publisher: Arc<Publisher>,
    history: Arc<RwLock<SlotHistory>>,
    info: Arc<RwLock<ValidatorInfoCache>>,
    epochs: Arc<RwLock<Vec<EpochInfo>>>,
    limits: Limits,
    allowed_hosts: &[String],
) -> Result<(), ConnectionError> {
    let (head, head_len) = timeout(REQUEST_TIMEOUT, peek_request_head(&socket))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request head"))??;

    // Taken after the head is read, so a refusal can drain and answer rather than
    // closing on unread bytes and sending a reset.
    let Ok(_connection) = limits.connections.try_acquire_owned() else {
        log::info!("dashboard: refusing a connection, {MAX_CONNECTIONS} already being served");
        return refuse(socket, head_len, 503, b"too many dashboard connections").await;
    };

    // Checked before anything is served: a page served to a rebound name would be
    // same-origin with the dashboard.
    if !host_is_allowed(&head, allowed_hosts) {
        log::debug!(
            "dashboard: refusing host {:?}; add it with --dashboard-allowed-host",
            for_logging(header(&head, "host").unwrap_or("(absent)"))
        );
        return refuse(socket, head_len, 421, b"unrecognised host").await;
    }

    if is_websocket_upgrade(&head) {
        if !origin_is_allowed(&head) {
            log::debug!(
                "dashboard: refusing websocket from origin {:?}",
                for_logging(header(&head, "origin").unwrap_or("(absent)"))
            );
            return refuse(socket, head_len, 403, b"origin not allowed").await;
        }
        // Held for the lifetime of the websocket.
        let Ok(_permit) = limits.websockets.try_acquire_owned() else {
            log::info!(
                "dashboard: refusing a websocket, {MAX_WEBSOCKET_CLIENTS} clients already \
                 connected"
            );
            return refuse(socket, head_len, 503, b"too many dashboard clients").await;
        };
        let path = request_path(&head).to_string();
        serve_websocket(socket, publisher, history, info, epochs, &path).await
    } else {
        // Consume the peeked bytes. Closing with unread data makes the kernel send
        // RST rather than FIN, which truncates a large response.
        let mut consumed = vec![0u8; head_len];
        socket.read_exact(&mut consumed).await?;
        serve_http(socket, &head)
            .await
            .map_err(ConnectionError::from)
    }
}

/// Turns a request away with a status rather than a silent close. Every
/// refusal happens before any upgrade, so the caller is still speaking HTTP.
async fn refuse(
    mut socket: TcpStream,
    head_len: usize,
    status: u16,
    body: &[u8],
) -> Result<(), ConnectionError> {
    // Same reason the HTTP path drains here: closing a socket with unread data
    // makes the kernel send RST, which discards the response we just wrote.
    let mut consumed = vec![0u8; head_len];
    socket.read_exact(&mut consumed).await?;
    socket
        .write_all(&response(status, "text/plain; charset=utf-8", body, false))
        .await?;
    socket.flush().await?;
    socket.shutdown().await?;
    Ok(())
}

/// Reads the request head without consuming it, so a websocket connection can
/// still be handed to soketto. Returns the head and its exact length, which the
/// HTTP path drains.
async fn peek_request_head(socket: &TcpStream) -> io::Result<(String, usize)> {
    let mut buffer = vec![0u8; MAX_REQUEST_HEAD];
    loop {
        socket.readable().await?;
        let peeked = match socket.peek(&mut buffer).await {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no request")),
            Ok(peeked) => peeked,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(err),
        };
        // `from_utf8_lossy` can change the byte count, so the length comes from
        // what was actually peeked rather than from the string.
        let head = String::from_utf8_lossy(&buffer[..peeked]);
        if head.contains("\r\n\r\n") || peeked == MAX_REQUEST_HEAD {
            return Ok((head.into_owned(), peeked));
        }
    }
}

/// Reads a request header, case-insensitively.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .skip(1) // the request line is not a header
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

/// The host part of an authority or origin, without scheme, port or brackets.
/// Ports are dropped: a dashboard proxied on another port is the same machine.
fn host_of(value: &str) -> &str {
    let value = value
        .rsplit_once("//")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    // IPv6 authorities are bracketed, so the port colon is the one outside.
    if let Some(rest) = value.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    value.split(':').next().unwrap_or(value)
}

/// Whether the request names a host this dashboard answers to. No `Host` at
/// all is rejected; every HTTP/1.1 client sends one.
fn host_is_allowed(head: &str, allowed: &[String]) -> bool {
    let Some(host) = header(head, "host") else {
        return false;
    };
    let host = host_of(host);

    // An address literal is always accepted. Rebinding works by resolving a name
    // the attacker owns, so the browser sends that name, never a bare address.
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }

    allowed
        .iter()
        .any(|candidate| host_of(candidate).eq_ignore_ascii_case(host))
}

/// Whether a websocket upgrade comes from a page served by this dashboard.
/// Websockets are exempt from the same-origin policy, so without this any page
/// a browser visits could read the feed, loopback included. A missing `Origin`
/// means the caller is not a browser and is allowed; a literal `null`, which a
/// sandboxed frame sends, is refused.
fn origin_is_allowed(head: &str) -> bool {
    let Some(origin) = header(head, "origin") else {
        return true;
    };
    if origin.eq_ignore_ascii_case("null") {
        return false;
    }
    match header(head, "host") {
        Some(host) => host_of(origin).eq_ignore_ascii_case(host_of(host)),
        None => false,
    }
}

fn is_websocket_upgrade(head: &str) -> bool {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("upgrade") && value.trim().eq_ignore_ascii_case("websocket")
        })
}

fn request_path(head: &str) -> &str {
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|target| target.split(['?', '#']).next().unwrap_or("/"))
        .unwrap_or("/")
}

// ---- static assets ------------------------------------------------------

async fn serve_http(mut socket: TcpStream, head: &str) -> io::Result<()> {
    let path = request_path(head);
    let is_read = head.starts_with("GET ") || head.starts_with("HEAD ");

    let response = if !is_read {
        response(
            405,
            "text/plain; charset=utf-8",
            b"method not allowed",
            false,
        )
    } else if assets::ASSETS.is_empty() {
        // Built without a frontend. Saying so beats a 404, which would read as
        // the server being broken.
        response(
            200,
            "text/html; charset=utf-8",
            MISSING_FRONTEND.as_bytes(),
            false,
        )
    } else {
        match lookup(path) {
            // Hashed asset filenames are safe to cache forever. The entry
            // document is not, or a redeploy would never be picked up.
            Some((content_type, body)) => {
                response(200, content_type, body, path.starts_with("/assets/"))
            }
            // Unknown paths fall through to the SPA so client-side routes
            // survive a hard refresh.
            None => match lookup("/index.html") {
                Some((content_type, body)) => response(200, content_type, body, false),
                None => response(404, "text/plain; charset=utf-8", b"not found", false),
            },
        }
    };

    socket.write_all(&response).await?;
    socket.flush().await?;
    // Shut the write half down explicitly so the peer sees a clean FIN after
    // the whole body, instead of whatever dropping the socket produces.
    socket.shutdown().await
}

fn lookup(path: &str) -> Option<(&'static str, &'static [u8])> {
    let path = if path == "/" { "/index.html" } else { path };
    assets::ASSETS
        .iter()
        .find(|(route, _, _)| *route == path)
        .map(|(_, content_type, body)| (*content_type, *body))
}

/// Sent with every response. `img-src` is open to any https host because
/// validator icons are URLs operators publish on chain; plaintext is refused.
/// Scripts and styles permit inline because index.html carries the theme stamp
/// and splash styling, and every value the client renders goes through React.
/// `connect-src 'self'` covers the websocket. One directive per line so it
/// formats the same everywhere.
const SECURITY_HEADERS: &str = concat!(
    "content-security-policy:",
    " default-src 'none';",
    " script-src 'self' 'unsafe-inline';",
    " style-src 'self' 'unsafe-inline';",
    " img-src 'self' data: https:;",
    " connect-src 'self';",
    " font-src 'self';",
    " base-uri 'none';",
    " form-action 'none';",
    " frame-ancestors 'self'\r\n",
    "x-content-type-options: nosniff\r\n",
    "referrer-policy: no-referrer\r\n",
);

fn response(status: u16, content_type: &str, body: &[u8], immutable: bool) -> Vec<u8> {
    let reason = match status {
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        421 => "Misdirected Request",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: \
         {}\r\ncache-control: {cache}\r\n{SECURITY_HEADERS}connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

// ---- websocket ----------------------------------------------------------

/// Fails the connection if a send cannot complete promptly. Cancelling a
/// partly written frame leaves the stream indeterminate, so a timeout is fatal.
macro_rules! send_or_timeout {
    ($expr:expr) => {
        timeout(WRITE_TIMEOUT, $expr)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "websocket send"))??
    };
}

async fn serve_websocket(
    socket: TcpStream,
    publisher: Arc<Publisher>,
    history: Arc<RwLock<SlotHistory>>,
    info: Arc<RwLock<ValidatorInfoCache>>,
    epochs: Arc<RwLock<Vec<EpochInfo>>>,
    path: &str,
) -> Result<(), ConnectionError> {
    let mut server = Server::new(socket.compat());
    // The request borrows the server, so the key is copied out and the borrow
    // dropped before the response goes back over that same server.
    let key = server.receive_request().await?.key();

    if path != WEBSOCKET_PATH {
        server
            .send_response(&server::Response::Reject { status_code: 404 })
            .await?;
        return Ok(());
    }
    server
        .send_response(&server::Response::Accept {
            key,
            protocol: None,
        })
        .await?;

    // Subscribing before taking the snapshot means a value that changes between
    // the two arrives as an update instead of going missing.
    let mut updates = publisher.subscribe();
    let snapshot = publisher.snapshot();

    // One limit, both directions. It has to clear the largest message the server
    // sends, and every byte above that is buffering a caller can cause.
    let mut builder = server.into_builder();
    builder.set_max_message_size(MAX_MESSAGE);
    builder.set_max_frame_size(MAX_MESSAGE);
    let (mut sender, mut receiver) = builder.finish();

    let total: usize = snapshot.iter().map(|message| message.len()).sum();
    for (index, message) in snapshot.iter().enumerate() {
        let sent = timeout(WRITE_TIMEOUT, sender.send_text(&**message))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "snapshot send"))?;
        if let Err(err) = sent {
            // Losing the snapshot leaves the client blank, so say exactly where it
            // stopped.
            log::warn!(
                "dashboard: snapshot send failed on message {} of {} ({} bytes, {total} bytes \
                 total, starts {:.120}): {err}",
                index.saturating_add(1),
                snapshot.len(),
                message.len(),
                message,
            );
            return Err(err.into());
        }
        // Flushing per message keeps one oversized entry from taking the whole
        // snapshot down with it.
        send_or_timeout!(sender.flush());
    }

    let mut incoming = Vec::new();
    loop {
        // soketto's `receive_data` is not cancel safe, so it is polled to
        // completion in an inner loop rather than dropped by `select!`.
        {
            let receive = receiver.receive_data(&mut incoming);
            pin!(receive);
            loop {
                select! {
                    // Client frames are handled first so that pings and closes
                    // are not starved by a busy update stream.
                    biased;

                    received = &mut receive => match received {
                        Ok(_) => break,
                        Err(soketto::connection::Error::Closed) => return Ok(()),
                        Err(err) => return Err(err.into()),
                    },

                    update = updates.recv() => match update {
                        Ok(message) => {
                            send_or_timeout!(sender.send_text(&*message));
                            // Drain whatever else is queued before flushing, so a burst costs one write.
                            loop {
                                match updates.try_recv() {
                                    Ok(message) => {
                                        send_or_timeout!(sender.send_text(&*message))
                                    }
                                    Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                                    // A client that cannot keep up is dropped rather than waited for.
                                    Err(TryRecvError::Lagged(_)) => {
                                        return Err(ConnectionError::Lagged);
                                    }
                                }
                            }
                            send_or_timeout!(sender.flush());
                        }
                        Err(RecvError::Lagged(_)) => return Err(ConnectionError::Lagged),
                        Err(RecvError::Closed) => return Ok(()),
                    },
                }
            }
        }

        // The connection-level limit has to be large enough for what the server
        // sends, so the bound on client messages is applied here instead.
        if incoming.len() > MAX_CLIENT_MESSAGE {
            return Err(ConnectionError::Oversized(incoming.len()));
        }
        if let Some(reply) = respond(&incoming, &history, &info, &epochs) {
            send_or_timeout!(sender.send_text(&*reply));
            send_or_timeout!(sender.flush());
        }
        incoming.clear();
    }
}

/// Bounds and flattens a caller-supplied string before it reaches the log. A
/// newline forges an entry, an escape sequence drives a terminal, a
/// right-to-left override reverses text, and an 8 KB header must not write an
/// 8 KB line.
fn for_logging(value: &str) -> String {
    const LIMIT: usize = 48;
    let mut out: String = value
        .chars()
        .take(LIMIT)
        .map(|c| match c {
            // Printable ASCII, space through tilde.
            ' '..='~' => c,
            _ => '?',
        })
        .collect();
    if value.chars().nth(LIMIT).is_some() {
        out.push_str("...");
    }
    out
}

/// Which epoch an `epoch.query` request is about.
#[derive(serde::Deserialize)]
struct EpochParams {
    epoch: u64,
}

/// What a `slot.range` request asks for.
#[derive(serde::Deserialize)]
struct SlotRangeParams {
    first_slot: Slot,
    /// Slots wanted from there on, oldest first. Clamped by the history rather
    /// than refused, so a client that asks for too many gets what fits.
    count: usize,
}

/// Handles a client request. Even unknown requests get an answer, so a client
/// is never left waiting on an id that will never come back.
fn respond(
    payload: &[u8],
    history: &RwLock<SlotHistory>,
    info: &RwLock<ValidatorInfoCache>,
    epochs: &RwLock<Vec<EpochInfo>>,
) -> Option<Message> {
    let request: Request = serde_json::from_slice(payload).ok()?;
    let id = request.id;
    match (request.topic.as_str(), request.key.as_str()) {
        ("summary", "ping") => Some(encode_with_id("summary", "ping", id, &())),
        ("summary", "displays") => {
            // Asked for rather than published: a hundred and fifty kilobytes on a cluster
            // this size, wanted only by a page that has searched into history. The whole
            // table rather than one epoch's leaders, since the cache is keyed by identity.
            let displays = match info.read() {
                Ok(info) => info.displays(),
                Err(_) => return Some(encode_with_id("summary", "displays", id, &())),
            };
            Some(encode_with_id("summary", "displays", id, &displays))
        }
        ("epoch", "query") => {
            let Ok(params) = serde_json::from_value::<EpochParams>(request.params) else {
                return Some(encode_with_id(
                    "epoch",
                    "query",
                    id,
                    &serde_json::json!({ "error": "query needs an epoch" }),
                ));
            };
            // Only this epoch and the one before it are held. Anything else is answered
            // with nothing rather than an error: a validator that has not been up that
            // long has no schedule for it.
            let found = match epochs.read() {
                Ok(epochs) => epochs
                    .iter()
                    .find(|held| held.epoch == params.epoch)
                    .cloned(),
                Err(_) => None,
            };
            Some(encode_with_id("epoch", "query", id, &found))
        }
        ("slot", "range") => {
            // Malformed parameters are answered rather than dropped: a client waiting on
            // an id cannot tell silence from slowness.
            let Ok(params) = serde_json::from_value::<SlotRangeParams>(request.params) else {
                return Some(encode_with_id(
                    "slot",
                    "range",
                    id,
                    &serde_json::json!({ "error": "range needs a first_slot and a count" }),
                ));
            };
            // Poisoned only if a collector thread panicked while holding it, in
            // which case the validator has larger problems than a blank list.
            let range = match history.read() {
                Ok(history) => history.range(params.first_slot, params.count),
                Err(_) => return Some(encode_with_id("slot", "range", id, &())),
            };
            Some(encode_with_id("slot", "range", id, &range))
        }
        (topic, key) => {
            log::debug!(
                "dashboard: unhandled request {:?}.{:?}",
                for_logging(topic),
                for_logging(key)
            );
            Some(encode_with_id(
                topic,
                key,
                id,
                &serde_json::json!({ "error": "unsupported request" }),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        soketto::handshake::{Client, ServerResponse},
        tokio_util::compat::Compat,
    };

    /// A history with nothing in it, which is what every test here wants: none
    /// of them is about the slots, and an empty one still answers a range.
    fn empty() -> RwLock<SlotHistory> {
        RwLock::new(SlotHistory::new(16))
    }

    fn empty_history() -> Arc<RwLock<SlotHistory>> {
        Arc::new(empty())
    }

    /// A cache nothing has been scanned into, which is what every test here
    /// wants: none of them is about the names, and an empty one still answers.
    fn no_info() -> RwLock<ValidatorInfoCache> {
        RwLock::new(ValidatorInfoCache::default())
    }

    fn no_info_shared() -> Arc<RwLock<ValidatorInfoCache>> {
        Arc::new(no_info())
    }

    /// No epoch held, which is a validator that has only just started. Every
    /// test here is about something else, and an empty archive still answers.
    fn no_epochs() -> RwLock<Vec<EpochInfo>> {
        RwLock::new(Vec::new())
    }

    fn no_epochs_shared() -> Arc<RwLock<Vec<EpochInfo>>> {
        Arc::new(no_epochs())
    }

    fn epoch_record(epoch: u64) -> EpochInfo {
        EpochInfo {
            epoch,
            start_slot: epoch.saturating_mul(432_000),
            end_slot: epoch.saturating_mul(432_000).saturating_add(431_999),
            slots_in_epoch: 432_000,
            my_leader_slots: Vec::new(),
            leaders: vec!["LEADER".to_string()],
            turns: vec![0],
            block_cost_limit: 60_000_000,
            account_cost_limit: 12_000_000,
        }
    }

    #[test]
    fn test_an_epoch_the_validator_still_holds_is_answered_with_its_arrays() {
        let epochs = RwLock::new(vec![epoch_record(841), epoch_record(842)]);
        let reply = respond(
            br#"{"topic":"epoch","key":"query","id":11,"params":{"epoch":841}}"#,
            &empty(),
            &no_info(),
            &epochs,
        )
        .unwrap();
        assert!(reply.contains(r#""id":11"#), "{reply}");
        assert!(reply.contains(r#""epoch":841"#), "{reply}");
        assert!(reply.contains(r#""LEADER""#), "{reply}");
    }

    #[test]
    fn test_an_epoch_older_than_the_validator_kept_is_answered_with_nothing() {
        // Not an error. A validator that has not been up that long has no schedule
        // for it, and the page draws those turns without a leader.
        let epochs = RwLock::new(vec![epoch_record(842)]);
        let reply = respond(
            br#"{"topic":"epoch","key":"query","id":12,"params":{"epoch":700}}"#,
            &empty(),
            &no_info(),
            &epochs,
        )
        .unwrap();
        assert!(reply.contains(r#""id":12"#), "{reply}");
        assert!(reply.contains(r#""value":null"#), "{reply}");
    }

    #[test]
    fn test_the_display_table_carries_what_a_validator_calls_itself() {
        use crate::validator_info::ValidatorInfo;
        let info = RwLock::new(ValidatorInfoCache::default());
        info.write().unwrap().insert(
            solana_pubkey::Pubkey::new_from_array([7; 32]),
            ValidatorInfo {
                name: Some("Lantern".to_string()),
                icon_url: Some("https://l/i.png".to_string()),
            },
        );

        let reply = respond(
            br#"{"topic":"summary","key":"displays","id":4}"#,
            &empty(),
            &info,
            &no_epochs(),
        )
        .unwrap();
        assert!(reply.contains(r#""id":4"#), "{reply}");
        assert!(reply.contains(r#""Lantern""#), "{reply}");
        assert!(reply.contains(r#""https://l/i.png""#), "{reply}");
    }

    #[test]
    fn test_a_validator_that_published_nothing_takes_no_room_in_the_table() {
        // Most of a cluster publishes neither a name nor an icon. Carrying them
        // as a key and two nulls each would be most of the table saying nothing.
        use crate::validator_info::ValidatorInfo;
        let info = RwLock::new(ValidatorInfoCache::default());
        info.write().unwrap().insert(
            solana_pubkey::Pubkey::new_from_array([9; 32]),
            ValidatorInfo::default(),
        );

        let reply = respond(
            br#"{"topic":"summary","key":"displays","id":5}"#,
            &empty(),
            &info,
            &no_epochs(),
        )
        .unwrap();
        assert!(reply.contains(r#""keys":[]"#), "{reply}");
    }

    #[test]
    fn test_a_range_request_is_answered_with_its_own_id() {
        let history = empty();
        let reply = respond(
            br#"{"topic":"slot","key":"range","id":9,"params":{"first_slot":4,"count":2}}"#,
            &history,
            &no_info(),
            &no_epochs(),
        )
        .unwrap();
        assert!(reply.contains(r#""id":9"#), "{reply}");
        assert!(reply.contains(r#""first_slot":4"#), "{reply}");
        assert!(reply.contains(r#""rows":[null,null]"#), "{reply}");
    }

    #[test]
    fn test_a_range_with_unusable_parameters_is_answered_rather_than_dropped() {
        // Silence and a slow answer look the same to a client waiting on an id,
        // so every request that parses as one gets something back.
        let reply = respond(
            br#"{"topic":"slot","key":"range","id":3,"params":{"first_slot":"soon"}}"#,
            &empty(),
            &no_info(),
            &no_epochs(),
        )
        .unwrap();
        assert!(reply.contains(r#""id":3"#), "{reply}");
        assert!(reply.contains("error"), "{reply}");
    }

    #[test]
    fn test_detects_a_websocket_upgrade() {
        assert!(is_websocket_upgrade(
            "GET /websocket HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n"
        ));
        assert!(is_websocket_upgrade(
            "GET / HTTP/1.1\r\nupgrade:  WebSocket \r\n\r\n"
        ));
        assert!(!is_websocket_upgrade("GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
    }

    fn req(headers: &str) -> String {
        format!("GET /websocket HTTP/1.1\r\n{headers}\r\n\r\n")
    }

    fn allowed() -> Vec<String> {
        vec![
            "localhost".into(),
            "127.0.0.1".into(),
            "dash.example.com".into(),
        ]
    }

    #[test]
    fn test_page_this_dashboard_served_may_open_a_socket() {
        assert!(origin_is_allowed(&req(
            "Host: dash.example.com\r\nOrigin: https://dash.example.com"
        )));
        // Behind a proxy the visitor's port and the dashboard's differ.
        assert!(origin_is_allowed(&req(
            "Host: dash.example.com\r\nOrigin: https://dash.example.com:443"
        )));
    }

    #[test]
    fn test_another_site_may_not() {
        // The whole point: a websocket is exempt from the same-origin policy,
        // so without this any page a browser visits could read the feed.
        assert!(!origin_is_allowed(&req(
            "Host: dash.example.com\r\nOrigin: https://evil.example"
        )));
        // Including against a dashboard assumed private for being on loopback.
        assert!(!origin_is_allowed(&req(
            "Host: 127.0.0.1:10999\r\nOrigin: https://evil.example"
        )));
    }

    #[test]
    fn test_sandboxed_frame_is_refused() {
        assert!(!origin_is_allowed(&req(
            "Host: dash.example.com\r\nOrigin: null"
        )));
    }

    #[test]
    fn test_client_that_is_not_a_browser_is_allowed() {
        // curl and monitoring send no Origin, and cannot be acting for a page.
        assert!(origin_is_allowed(&req("Host: dash.example.com")));
    }

    #[test]
    fn test_only_known_hosts_are_answered() {
        assert!(host_is_allowed(&req("Host: localhost:10999"), &allowed()));
        assert!(host_is_allowed(&req("Host: DASH.EXAMPLE.COM"), &allowed()));
        assert!(!host_is_allowed(&req("Host: rebind.evil"), &allowed()));
        assert!(!host_is_allowed(&req("Origin: x"), &allowed()));
    }

    #[test]
    fn test_address_literal_needs_no_configuration() {
        // Testing on a public IP before any domain exists must just work: an
        // address cannot be rebound, so nothing is being relaxed here.
        assert!(host_is_allowed(&req("Host: 111.1.1.1:10999"), &allowed()));
        assert!(host_is_allowed(&req("Host: 127.0.0.1:10999"), &allowed()));
        assert!(host_is_allowed(&req("Host: [::1]:10999"), &allowed()));
        assert!(host_is_allowed(&req("Host: [2001:db8::1]"), &allowed()));
        // With an empty allowlist too, since the defaults name only localhost.
        assert!(host_is_allowed(&req("Host: 111.1.1.1"), &[]));
    }

    #[test]
    fn test_name_still_has_to_be_named() {
        // The rebinding defence survives the concession above: what an
        // attacker controls is a name, and a name is still checked.
        assert!(!host_is_allowed(&req("Host: rebind.evil"), &[]));
        assert!(!host_is_allowed(
            &req("Host: 111.1.1.1.evil.com"),
            &allowed()
        ));
        assert!(!host_is_allowed(&req("Host: 999.999.999.999"), &allowed()));
    }

    #[test]
    fn test_more_than_one_name_can_be_allowed() {
        let hosts = vec!["a.example.com".to_string(), "b.example.com".to_string()];
        assert!(host_is_allowed(&req("Host: a.example.com"), &hosts));
        assert!(host_is_allowed(&req("Host: b.example.com"), &hosts));
        assert!(!host_is_allowed(&req("Host: c.example.com"), &hosts));
    }

    #[test]
    fn test_rebinding_defeats_the_origin_check_and_is_caught_by_the_host_check() {
        let rebound = req("Host: rebind.evil:10999\r\nOrigin: http://rebind.evil");
        assert!(
            origin_is_allowed(&rebound),
            "origin matches host under rebinding, which is why the host check exists"
        );
        assert!(!host_is_allowed(&rebound, &allowed()));
    }

    #[test]
    fn test_hosts_are_compared_without_scheme_or_port() {
        assert_eq!(host_of("https://dash.example.com:443"), "dash.example.com");
        assert_eq!(host_of("dash.example.com:10999"), "dash.example.com");
        assert_eq!(host_of("[::1]:10999"), "::1");
        assert_eq!(host_of("http://[::1]"), "::1");
    }

    #[test]
    fn test_headers_are_read_case_insensitively_and_stop_at_the_body() {
        let head = "GET / HTTP/1.1\r\nHOST: x\r\n\r\nHost: injected\r\n";
        assert_eq!(header(head, "host"), Some("x"));
        assert_eq!(header(head, "missing"), None);
    }

    #[test]
    fn test_extracts_the_request_path() {
        assert_eq!(
            request_path("GET /assets/app.js HTTP/1.1\r\n"),
            "/assets/app.js"
        );
        assert_eq!(request_path("GET /?x=1 HTTP/1.1\r\n"), "/");
        assert_eq!(request_path("garbage"), "/");
    }

    /// Drives `handle` against a real socket with the cap already taken, which
    /// is the state a 65th viewer would arrive in.
    async fn request_with_no_permits_left(request: &[u8]) -> String {
        // These fixtures use `Host: x`, so the host policy is widened to match.
        // The cap is what is under test here, not which hosts are answered.
        let allowed_hosts = vec!["x".to_string()];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let limits = Limits {
            connections: Arc::new(Semaphore::new(1)),
            websockets: Arc::new(Semaphore::new(1)),
        };
        // Stands in for a viewer already holding the only slot.
        let _held = limits.websockets.clone().try_acquire_owned().unwrap();

        let publisher = Arc::new(Publisher::new());
        let server = tokio::spawn({
            let limits = limits.clone();
            async move {
                let (socket, _) = listener.accept().await.unwrap();
                handle(
                    socket,
                    publisher,
                    empty_history(),
                    no_info_shared(),
                    no_epochs_shared(),
                    limits,
                    &allowed_hosts,
                )
                .await
            }
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        server.await.unwrap().unwrap();
        reply
    }

    /// Drives `handle` end to end with a named host that is not allowed, which
    /// is what a rebinding attempt looks like on the wire.
    async fn request_with_hosts(request: &[u8], allowed_hosts: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let publisher = Arc::new(Publisher::new());
        let limits = Limits::new();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            handle(
                socket,
                publisher,
                empty_history(),
                no_info_shared(),
                no_epochs_shared(),
                limits,
                &allowed_hosts,
            )
            .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        server.await.unwrap().unwrap();
        reply
    }

    #[tokio::test]
    async fn test_unrecognised_host_is_turned_away_before_anything_is_served() {
        let reply = request_with_hosts(
            b"GET / HTTP/1.1\r\nHost: rebind.evil\r\n\r\n",
            vec!["dash.example.com".to_string()],
        )
        .await;
        assert!(
            reply.starts_with("HTTP/1.1 421 Misdirected Request"),
            "expected a refusal, got {reply:?}"
        );
        // The page itself must not have gone out: serving it would make the
        // attacker's origin same-origin with the dashboard.
        assert!(!reply.contains("<!doctype"), "document was served anyway");
    }

    #[tokio::test]
    async fn test_websocket_from_another_origin_is_refused() {
        let reply = request_with_hosts(
            b"GET /websocket HTTP/1.1\r\nHost: dash.example.com\r\nUpgrade: websocket\r\nOrigin: https://evil.example\r\n\r\n",
            vec!["dash.example.com".to_string()],
        )
        .await;
        assert!(
            reply.starts_with("HTTP/1.1 403 Forbidden"),
            "expected a refusal, got {reply:?}"
        );
    }

    #[tokio::test]
    async fn test_websocket_over_the_cap_is_refused_with_a_status() {
        let reply = request_with_no_permits_left(
            b"GET /websocket HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n",
        )
        .await;
        assert!(
            reply.starts_with("HTTP/1.1 503 Service Unavailable"),
            "expected a refusal, got {reply:?}"
        );
    }

    #[tokio::test]
    async fn test_http_is_still_served_when_the_websocket_cap_is_full() {
        // The point of capping websockets alone: a full pool must not stop the
        // page itself from loading.
        let reply = request_with_no_permits_left(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            reply.starts_with("HTTP/1.1 200 OK"),
            "expected the page, got {:?}",
            &reply[..reply.len().min(80)]
        );
    }

    /// Drives `handle` with the connection cap already exhausted, which is the
    /// state every request arrives in once a flood has filled it.
    async fn request_with_no_connections_left(request: &[u8]) -> String {
        let allowed_hosts = vec!["x".to_string()];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let limits = Limits {
            connections: Arc::new(Semaphore::new(1)),
            websockets: Arc::new(Semaphore::new(MAX_WEBSOCKET_CLIENTS)),
        };
        let _held = limits.connections.clone().try_acquire_owned().unwrap();

        let publisher = Arc::new(Publisher::new());
        let server = tokio::spawn({
            let limits = limits.clone();
            async move {
                let (socket, _) = listener.accept().await.unwrap();
                handle(
                    socket,
                    publisher,
                    empty_history(),
                    no_info_shared(),
                    no_epochs_shared(),
                    limits,
                    &allowed_hosts,
                )
                .await
            }
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        server.await.unwrap().unwrap();
        reply
    }

    #[tokio::test]
    async fn test_full_connection_cap_refuses_rather_than_serving() {
        // A refusal has to arrive as a complete response, not a reset, or the caller
        // cannot tell an overloaded dashboard from a broken one.
        let reply = request_with_no_connections_left(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            reply.starts_with("HTTP/1.1 503 Service Unavailable"),
            "expected a refusal, got {:?}",
            &reply[..reply.len().min(80)]
        );
        assert!(reply.contains("too many dashboard connections"));
    }

    #[tokio::test]
    async fn test_connection_cap_covers_websockets_too() {
        // A websocket is a connection. If it were exempt, the cheapest way past
        // the cap would be to open the long-lived kind.
        let reply = request_with_no_connections_left(
            b"GET /websocket HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n",
        )
        .await;
        assert!(
            reply.starts_with("HTTP/1.1 503"),
            "expected a refusal, got {:?}",
            &reply[..reply.len().min(80)]
        );
    }

    /// The status line of a response, as a client would read it.
    fn status_line(status: u16, immutable: bool) -> String {
        let out = String::from_utf8(response(status, "text/plain", b"", immutable)).unwrap();
        out.lines().next().unwrap().to_string()
    }

    #[test]
    fn test_every_status_the_server_sends_has_its_reason_phrase() {
        // Every one of these is reachable. A status with the wrong phrase is what a
        // proxy logs and nobody reads until it matters.
        assert_eq!(status_line(200, false), "HTTP/1.1 200 OK");
        assert_eq!(status_line(403, false), "HTTP/1.1 403 Forbidden");
        assert_eq!(status_line(404, false), "HTTP/1.1 404 Not Found");
        assert_eq!(status_line(405, false), "HTTP/1.1 405 Method Not Allowed");
        assert_eq!(status_line(421, false), "HTTP/1.1 421 Misdirected Request");
        assert_eq!(status_line(503, false), "HTTP/1.1 503 Service Unavailable");
    }

    #[test]
    fn test_an_unlisted_status_still_produces_a_usable_line() {
        // The fallback arm.
        assert_eq!(status_line(418, false), "HTTP/1.1 418 OK");
    }

    #[test]
    fn test_hashed_assets_are_cached_forever_and_the_document_never_is() {
        // Asset filenames carry a content hash; index.html does not, and caching it
        // would hide a redeploy.
        assert!(
            String::from_utf8(response(200, "text/javascript", b"x", true))
                .unwrap()
                .contains("cache-control: public, max-age=31536000, immutable")
        );
        assert!(
            String::from_utf8(response(200, "text/html", b"x", false))
                .unwrap()
                .contains("cache-control: no-cache")
        );
    }

    #[test]
    fn test_the_content_length_counts_the_body_that_follows() {
        // A length that disagrees with the body leaves the client waiting, or reading
        // the next response as this one's tail.
        for body in [b"".as_slice(), b"x".as_slice(), b"hello world".as_slice()] {
            let out = response(200, "text/plain", body, false);
            let text = String::from_utf8(out.clone()).unwrap();
            assert!(
                text.contains(&format!("content-length: {}\r\n", body.len())),
                "missing or wrong length for {body:?}"
            );
            let (head, sent) = text.split_once("\r\n\r\n").expect("no header terminator");
            assert_eq!(sent.as_bytes(), body);
            assert_eq!(out.len(), head.len() + 4 + body.len());
        }
    }

    #[test]
    fn test_responses_carry_the_security_headers() {
        let out = String::from_utf8(response(200, "text/html", b"<html>", false)).unwrap();
        assert!(out.contains("content-security-policy:"));
        assert!(out.contains("x-content-type-options: nosniff"));
        assert!(out.contains("referrer-policy: no-referrer"));
    }

    #[test]
    fn test_policy_still_permits_validator_icons() {
        // Icons are third-party by nature; only plaintext is refused.
        assert!(SECURITY_HEADERS.contains("img-src 'self' data: https:"));
        assert!(!SECURITY_HEADERS.contains("img-src 'self'\r\n"));
    }

    #[test]
    fn test_header_block_is_well_formed() {
        let out = String::from_utf8(response(200, "text/plain", b"body", false)).unwrap();
        // Exactly one blank line, and it separates headers from body.
        let (head, body) = out.split_once("\r\n\r\n").expect("no header terminator");
        assert_eq!(body, "body");
        assert!(!head.contains("\r\n\r\n"), "blank line inside the headers");
        for line in head.split("\r\n").skip(1) {
            assert!(line.contains(':'), "malformed header line: {line:?}");
        }
    }

    #[test]
    fn test_ping_is_answered_with_its_id() {
        let reply = respond(
            br#"{"topic":"summary","key":"ping","id":7}"#,
            &empty(),
            &no_info(),
            &no_epochs(),
        )
        .unwrap();
        assert!(reply.contains(r#""id":7"#));
    }

    #[test]
    fn test_unknown_requests_still_get_a_reply() {
        let reply = respond(
            br#"{"topic":"nope","key":"nope","id":1}"#,
            &empty(),
            &no_info(),
            &no_epochs(),
        )
        .unwrap();
        assert!(reply.contains("unsupported request"));
    }

    #[test]
    fn test_malformed_requests_are_ignored() {
        assert!(respond(b"not json", &empty(), &no_info(), &no_epochs()).is_none());
    }

    #[test]
    fn test_log_text_cannot_forge_a_second_line() {
        // A newline would end the entry and let the rest pose as one the
        // validator wrote itself.
        assert_eq!(
            for_logging("summary\n[INFO] dashboard: all good"),
            "summary?[INFO] dashboard: all good"
        );
        // An escape sequence would reach the terminal of whoever tails the log,
        // and an override would reverse the text printed after it.
        assert_eq!(for_logging("\u{1b}[2Jwiped"), "?[2Jwiped");
        assert_eq!(for_logging("nice\u{202e}gnp.exe"), "nice?gnp.exe");
    }

    #[test]
    fn test_log_text_is_bounded() {
        let logged = for_logging(&"a".repeat(4096));
        assert!(
            logged.len() < 64,
            "unbounded log line: {} bytes",
            logged.len()
        );
        assert!(
            logged.ends_with("..."),
            "truncation is not visible: {logged:?}"
        );
        // Anything short enough is passed through whole, with no marker.
        assert_eq!(for_logging("summary"), "summary");
    }

    #[tokio::test]
    async fn test_a_get_over_a_socket_serves_the_page() {
        // The whole HTTP path end to end. The drain is the part that matters: closing
        // on unread bytes sends a reset that discards the response.
        let reply = request_with_hosts(
            b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            vec!["localhost".to_string()],
        )
        .await;
        assert!(
            reply.starts_with("HTTP/1.1 200 OK"),
            "expected the page, got {:?}",
            &reply[..reply.len().min(80)]
        );
        assert!(reply.contains("content-type: text/html"));
        assert!(
            reply.contains("\r\n\r\n"),
            "a response with no body terminator"
        );
    }

    #[tokio::test]
    async fn test_an_unknown_path_falls_through_to_the_app() {
        // Client-side routes have to survive a hard refresh, so anything not
        // found is answered with the entry document rather than a 404.
        let reply = request_with_hosts(
            b"GET /some/client/route HTTP/1.1\r\nHost: localhost\r\n\r\n",
            vec!["localhost".to_string()],
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 200 OK"));
        assert!(reply.contains("content-type: text/html"));
    }

    #[tokio::test]
    async fn test_a_method_that_would_write_is_refused() {
        // Nothing here accepts input over HTTP, and a server that quietly
        // ignores a POST reads as one that took it.
        let reply = request_with_hosts(
            b"POST / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            vec!["localhost".to_string()],
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 405 Method Not Allowed"));
    }

    /// Serves exactly one connection, and hands back where to reach it.
    async fn serve_one(
        publisher: Arc<Publisher>,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<Result<(), ConnectionError>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let allowed_hosts = vec!["x".to_string()];
            let (socket, _) = listener.accept().await.unwrap();
            handle(
                socket,
                publisher,
                empty_history(),
                no_info_shared(),
                no_epochs_shared(),
                Limits::new(),
                &allowed_hosts,
            )
            .await
        });
        (addr, server)
    }

    /// Connects a real client and completes the handshake.
    async fn connect(addr: std::net::SocketAddr) -> Client<'static, Compat<TcpStream>> {
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = Client::new(stream.compat(), "x", WEBSOCKET_PATH);
        match client.handshake().await.unwrap() {
            ServerResponse::Accepted { .. } => {}
            _ => panic!("the server refused a well-formed handshake"),
        }
        client
    }

    #[tokio::test]
    async fn test_a_value_changing_reaches_a_connected_client() {
        // The update arm of the select loop, which a server that only ever sent the
        // snapshot would have passed every other test without.
        let publisher = Arc::new(Publisher::new());
        publisher.publish("summary", "cluster", &"testnet");
        let (addr, server) = serve_one(publisher.clone()).await;
        let (mut sender, mut receiver) = connect(addr).await.into_builder().finish();

        // Reading the snapshot first proves the subscription exists before the
        // publish below.
        let mut first = Vec::new();
        receiver.receive_data(&mut first).await.unwrap();

        publisher.publish("summary", "root_slot", &42u64);
        let mut update = Vec::new();
        receiver.receive_data(&mut update).await.unwrap();
        let update = String::from_utf8(update).unwrap();
        assert!(
            update.contains(r#""key":"root_slot""#) && update.contains(r#""value":42"#),
            "expected the update, got {update}"
        );

        sender.close().await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_a_client_request_is_answered_on_the_same_socket() {
        // Client frames are polled ahead of the update stream, so a request is
        // answered even while values are moving.
        let publisher = Arc::new(Publisher::new());
        publisher.publish("summary", "cluster", &"testnet");
        let (addr, server) = serve_one(publisher.clone()).await;
        let (mut sender, mut receiver) = connect(addr).await.into_builder().finish();

        let mut snapshot = Vec::new();
        receiver.receive_data(&mut snapshot).await.unwrap();

        sender
            .send_text(r#"{"topic":"summary","key":"ping","id":7}"#)
            .await
            .unwrap();
        sender.flush().await.unwrap();

        let mut reply = Vec::new();
        receiver.receive_data(&mut reply).await.unwrap();
        let reply = String::from_utf8(reply).unwrap();
        assert!(
            reply.contains(r#""id":7"#),
            "the reply did not carry the request's id: {reply}"
        );

        sender.close().await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_an_oversized_client_message_ends_the_connection() {
        // The connection-level limit has to clear the largest server message, so the
        // client bound is applied by hand after the frame arrives.
        let publisher = Arc::new(Publisher::new());
        publisher.publish("summary", "cluster", &"testnet");
        let (addr, server) = serve_one(publisher.clone()).await;
        let (mut sender, mut receiver) = connect(addr).await.into_builder().finish();

        let mut snapshot = Vec::new();
        receiver.receive_data(&mut snapshot).await.unwrap();

        let oversized = "x".repeat(MAX_CLIENT_MESSAGE + 1);
        sender.send_text(&oversized).await.unwrap();
        sender.flush().await.unwrap();

        match server.await.unwrap() {
            Err(ConnectionError::Oversized(len)) => {
                assert!(len > MAX_CLIENT_MESSAGE, "reported {len} bytes")
            }
            other => panic!("expected the connection to be dropped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_a_websocket_client_is_sent_the_retained_snapshot() {
        // A client connecting at any moment is caught up in one shot. Driven with a
        // real soketto client so the handshake and framing are exercised.
        let publisher = Arc::new(Publisher::new());
        publisher.publish("summary", "cluster", &"testnet");
        publisher.publish("summary", "root_slot", &7u64);
        let (addr, server) = serve_one(publisher.clone()).await;

        let (mut sender, mut receiver) = connect(addr).await.into_builder().finish();
        let mut frames = Vec::new();
        for _ in 0..2 {
            let mut data = Vec::new();
            receiver.receive_data(&mut data).await.unwrap();
            frames.push(String::from_utf8(data).unwrap());
        }

        assert!(
            frames
                .iter()
                .any(|frame| frame.contains(r#""key":"cluster""#)),
            "the snapshot was missing a retained key: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains(r#""key":"root_slot""#)),
            "the snapshot was missing a retained key: {frames:?}"
        );

        // A clean close, so the server returns rather than being torn down.
        sender.close().await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[test]
    fn test_root_serves_the_entry_document() {
        // Only meaningful when the frontend was built; otherwise the server
        // serves the placeholder page and there is nothing to look up.
        if !assets::ASSETS.is_empty() {
            assert!(lookup("/").is_some());
        }
    }
}
