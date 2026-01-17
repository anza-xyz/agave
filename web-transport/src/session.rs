use std::{
    future::{poll_fn, Future},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Context, Poll},
};

use futures::stream::{FuturesUnordered, Stream, StreamExt};
use web_transport_proto::{StreamUni, VarInt};

use crate::{
    connect::Connect, recv::RecvStream, settings::Settings, SessionError, WebTransportError,
};

type PendingUni = Pin<Box<dyn Future<Output = Result<quinn::RecvStream, SessionError>> + Send>>;

#[derive(Clone)]
pub struct Session {
    conn: quinn::Connection,
    _session_id: VarInt,
    accept: Arc<Mutex<SessionAccept>>,

    #[allow(dead_code)]
    _settings: Arc<Settings>,
}

impl Session {
    pub(crate) fn new(conn: quinn::Connection, settings: Settings, connect: Connect) -> Self {
        let session_id = connect.session_id();
        let accept = SessionAccept::new(conn.clone(), session_id);

        let this = Self {
            conn,
            _session_id: session_id,
            accept: Arc::new(Mutex::new(accept)),
            _settings: Arc::new(settings),
        };

        let conn_clone = this.conn.clone();
        tokio::spawn(async move {
            let (code, reason) = Self::run_closed(connect).await;
            let code = web_transport_proto::error_to_http3(code)
                .try_into()
                .unwrap();
            conn_clone.close(code, reason.as_bytes());
        });

        this
    }

    async fn run_closed(connect: Connect) -> (u32, String) {
        let (_send, mut recv) = connect.into_inner();

        loop {
            match web_transport_proto::Capsule::read(&mut recv).await {
                Ok(web_transport_proto::Capsule::CloseWebTransportSession { code, reason }) => {
                    return (code, reason);
                }
                Ok(web_transport_proto::Capsule::Unknown { .. }) => continue,
                Err(_) => return (1, "capsule error".to_string()),
            }
        }
    }

    pub async fn accept_uni(&self) -> Result<RecvStream, SessionError> {
        poll_fn(|cx| self.accept.lock().unwrap().poll_accept_uni(cx)).await
    }

    pub fn remote_address(&self) -> std::net::SocketAddr {
        self.conn.remote_address()
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        let code = web_transport_proto::error_to_http3(code)
            .try_into()
            .unwrap();
        self.conn.close(code, reason)
    }

    pub async fn closed(&self) -> SessionError {
        self.conn.closed().await.into()
    }
}

struct SessionAccept {
    session_id: VarInt,
    accept_uni:
        Pin<Box<dyn Stream<Item = Result<quinn::RecvStream, quinn::ConnectionError>> + Send>>,
    pending_uni: FuturesUnordered<PendingUni>,
}

impl SessionAccept {
    fn new(conn: quinn::Connection, session_id: VarInt) -> Self {
        Self {
            session_id,
            accept_uni: Box::pin(futures::stream::unfold(conn, |conn| async move {
                Some((conn.accept_uni().await, conn))
            })),
            pending_uni: FuturesUnordered::new(),
        }
    }

    fn poll_accept_uni(&mut self, cx: &mut Context<'_>) -> Poll<Result<RecvStream, SessionError>> {
        loop {
            if let Poll::Ready(Some(res)) = self.accept_uni.poll_next_unpin(cx) {
                let recv = res?;
                let session_id = self.session_id;
                self.pending_uni
                    .push(Box::pin(Self::decode_uni(recv, session_id)));
                continue;
            }

            let res = match ready!(self.pending_uni.poll_next_unpin(cx)) {
                Some(Ok(recv)) => recv,
                Some(Err(err)) => {
                    log::warn!("failed to decode uni stream: {err:?}");
                    continue;
                }
                None => return Poll::Pending,
            };

            return Poll::Ready(Ok(RecvStream::new(res)));
        }
    }

    async fn decode_uni(
        mut recv: quinn::RecvStream,
        expected_session: VarInt,
    ) -> Result<quinn::RecvStream, SessionError> {
        let typ = VarInt::read(&mut recv)
            .await
            .map_err(|_| WebTransportError::UnknownSession)?;

        if StreamUni(typ) != StreamUni::WEBTRANSPORT {
            return Err(WebTransportError::UnknownSession.into());
        }

        let session_id = VarInt::read(&mut recv)
            .await
            .map_err(|_| WebTransportError::UnknownSession)?;

        if session_id != expected_session {
            return Err(WebTransportError::UnknownSession.into());
        }

        Ok(recv)
    }
}
