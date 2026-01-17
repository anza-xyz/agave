use crate::{ReadError, ReadToEndError};

#[derive(Debug)]
pub struct RecvStream {
    inner: quinn::RecvStream,
}

impl RecvStream {
    pub(crate) fn new(stream: quinn::RecvStream) -> Self {
        Self { inner: stream }
    }

    pub async fn read_to_end(&mut self, size_limit: usize) -> Result<Vec<u8>, ReadToEndError> {
        self.inner.read_to_end(size_limit).await.map_err(Into::into)
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, ReadError> {
        self.inner.read(buf).await.map_err(Into::into)
    }

    pub fn stop(&mut self, code: u32) -> Result<(), quinn::ClosedStream> {
        let code = web_transport_proto::error_to_http3(code);
        let code = quinn::VarInt::try_from(code).unwrap();
        self.inner.stop(code)
    }
}
