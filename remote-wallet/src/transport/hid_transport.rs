use super::transport_trait::Transport;

pub struct HidTransport {
    pub device: hidapi::HidDevice,
}

impl HidTransport {
    pub fn new(device: hidapi::HidDevice) -> Self {
        Self { device }
    }
}

impl Transport for HidTransport {
    fn write(&self, data: &[u8]) -> Result<usize, String> {
        self.device.write(data).map_err(|e| e.to_string())
    }
    fn read(&self) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; 64];
        let len = self.device.read(&mut buf).map_err(|e| e.to_string())?;
        buf.truncate(len);
        Ok(buf)
    }
}
