use crate::errors::RemoteWalletError;
pub trait Transport: Send {
    fn connect(&mut self) -> Result<(), RemoteWalletError>;

    fn disconnect(&mut self);

    fn is_connected(&self) -> bool;

    fn write(&self, data: &[u8]) -> Result<usize, RemoteWalletError>;

    fn read(&self) -> Result<Vec<u8>, RemoteWalletError>;
}
