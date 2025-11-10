use crate::locator::LocatorError;
use crate::wallet::ledger::error::LedgerError;
use solana_derivation_path::DerivationPathError;
use solana_signer::SignerError;
use thiserror::Error;

/// Remote wallet error.
#[derive(Error, Debug, Clone)]
pub enum RemoteWalletError {
    #[error("hidapi error")]
    Hid(String),

    #[error("device type mismatch")]
    DeviceTypeMismatch,

    #[error("device with non-supported product ID or vendor ID was detected")]
    InvalidDevice,

    #[error(transparent)]
    DerivationPathError(#[from] DerivationPathError),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error(transparent)]
    LedgerError(#[from] LedgerError),

    #[error("no device found")]
    NoDeviceFound,

    #[error("protocol error: {0}")]
    Protocol(&'static str),

    #[error("pubkey not found for given address")]
    PubkeyNotFound,

    #[error("remote wallet operation rejected by the user")]
    UserCancel,

    #[error(transparent)]
    LocatorError(#[from] LocatorError),
}

#[cfg(feature = "hidapi")]
impl From<hidapi::HidError> for RemoteWalletError {
    fn from(err: hidapi::HidError) -> RemoteWalletError {
        RemoteWalletError::Hid(err.to_string())
    }
}

impl From<RemoteWalletError> for SignerError {
    fn from(err: RemoteWalletError) -> SignerError {
        match err {
            RemoteWalletError::Hid(hid_error) => SignerError::Connection(hid_error),
            RemoteWalletError::DeviceTypeMismatch => SignerError::Connection(err.to_string()),
            RemoteWalletError::InvalidDevice => SignerError::Connection(err.to_string()),
            RemoteWalletError::InvalidInput(input) => SignerError::InvalidInput(input),
            RemoteWalletError::LedgerError(e) => SignerError::Protocol(e.to_string()),
            RemoteWalletError::NoDeviceFound => SignerError::NoDeviceFound,
            RemoteWalletError::Protocol(e) => SignerError::Protocol(e.to_string()),
            RemoteWalletError::UserCancel => {
                SignerError::UserCancel("remote wallet operation rejected by the user".to_string())
            }
            _ => SignerError::Custom(err.to_string()),
        }
    }
}
