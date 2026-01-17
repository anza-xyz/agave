use futures::try_join;

use crate::SettingsError;

pub struct Settings {
    #[allow(dead_code)]
    send: quinn::SendStream,

    #[allow(dead_code)]
    recv: quinn::RecvStream,
}

impl Settings {
    pub async fn connect(conn: &quinn::Connection) -> Result<Self, SettingsError> {
        let recv = Self::accept(conn);
        let send = Self::open(conn);

        let (send, recv) = try_join!(send, recv)?;
        Ok(Self { send, recv })
    }

    async fn accept(conn: &quinn::Connection) -> Result<quinn::RecvStream, SettingsError> {
        let mut recv = conn.accept_uni().await?;
        let settings = web_transport_proto::Settings::read(&mut recv).await?;

        log::debug!("received SETTINGS frame: {settings:?}");

        if settings.supports_webtransport() == 0 {
            return Err(SettingsError::WebTransportUnsupported);
        }

        Ok(recv)
    }

    async fn open(conn: &quinn::Connection) -> Result<quinn::SendStream, SettingsError> {
        let mut settings = web_transport_proto::Settings::default();
        settings.enable_webtransport(1);

        log::debug!("sending SETTINGS frame: {settings:?}");

        let mut send = conn.open_uni().await?;
        settings.write(&mut send).await?;

        Ok(send)
    }
}
