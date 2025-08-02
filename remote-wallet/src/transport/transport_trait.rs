pub trait Transport: Send {
    fn write(&self, data: &[u8]) -> Result<usize, String>;
    fn read(&self) -> Result<Vec<u8>, String>;
}
