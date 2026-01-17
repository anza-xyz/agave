use web_transport_proto::{ConnectRequest, ConnectResponse, VarInt};

use crate::ConnectError;

pub struct Connect {
    request: ConnectRequest,
    send: quinn::SendStream,

    #[allow(dead_code)]
    recv: quinn::RecvStream,
}

impl Connect {
    pub async fn accept(conn: &quinn::Connection) -> Result<Self, ConnectError> {
        let (send, mut recv) = conn.accept_bi().await?;

        let request = ConnectRequest::read(&mut recv).await?;
        log::debug!("received CONNECT request: {request:?}");

        Ok(Self {
            request,
            send,
            recv,
        })
    }

    pub async fn respond(&mut self, status: http::StatusCode) -> Result<(), ConnectError> {
        let resp = ConnectResponse { status };

        log::debug!("sending CONNECT response: {resp:?}");
        resp.write(&mut self.send).await?;

        Ok(())
    }

    pub fn session_id(&self) -> VarInt {
        let stream_id = quinn::VarInt::from(self.send.id());
        VarInt::try_from(stream_id.into_inner()).unwrap()
    }

    pub fn url(&self) -> &url::Url {
        &self.request.url
    }

    pub fn into_inner(self) -> (quinn::SendStream, quinn::RecvStream) {
        (self.send, self.recv)
    }
}
