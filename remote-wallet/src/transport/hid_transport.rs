use super::transport_trait::Transport;
use crate::errors::RemoteWalletError;

pub struct HidTransport {
    pub device: hidapi::HidDevice,
}

impl HidTransport {
    pub fn new(device: hidapi::HidDevice) -> Self {
        Self { device }
    }
}

impl Transport for HidTransport {
    fn connect(&mut self) -> Result<(), RemoteWalletError> {
        Ok(())
    }

    fn disconnect(&mut self) {}

    fn is_connected(&self) -> bool {
        true
    }

    fn write(&self, data: &[u8]) -> Result<usize, RemoteWalletError> {
        self.device
            .write(data)
            .map_err(|e| RemoteWalletError::Hid(e.to_string()))
    }

    fn read(&self) -> Result<Vec<u8>, RemoteWalletError> {
        let mut buf = vec![0u8; 64];
        let len = self
            .device
            .read(&mut buf)
            .map_err(|e| RemoteWalletError::Hid(e.to_string()))?;
        buf.truncate(len);
        Ok(buf)
    }
}
