use {thiserror::Error, trezor_client::error::Error as TrezorClientError};

#[derive(Error, Debug)]
pub enum TrezorError {
    #[error(transparent)]
    TrezorError(#[from] TrezorClientError),
}

impl Clone for TrezorError {
    fn clone(&self) -> Self {
        unimplemented!();
    }
}
