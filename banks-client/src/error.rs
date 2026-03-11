use {
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::{TransactionError, TransportError},
    std::io,
    tarpc::client::RpcError,
    thiserror::Error,
};

/// Errors from BanksClient
#[derive(Error, Debug)]
pub enum BanksClientError {
    #[error("client error: {0}")]
    ClientError(&'static str),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    RpcError(#[from] RpcError),

    #[error("transport transaction error: {0}")]
    TransactionError(#[from] TransactionError),

    #[error("simulation error: {err:?}, logs: {logs:?}, units_consumed: {units_consumed:?}")]
    SimulationError {
        err: TransactionError,
        logs: Vec<String>,
        units_consumed: u64,
        return_data: Option<TransactionReturnData>,
    },
}

impl BanksClientError {
    pub fn unwrap(&self) -> TransactionError {
        match self {
            BanksClientError::TransactionError(err)
            | BanksClientError::SimulationError { err, .. } => err.clone(),
            _ => panic!("unexpected transport error"),
        }
    }
}

impl From<BanksClientError> for io::Error {
    fn from(err: BanksClientError) -> Self {
        match err {
            BanksClientError::ClientError(err) => Self::other(err.to_string()),
            BanksClientError::Io(err) => err,
            BanksClientError::RpcError(err) => Self::other(err.to_string()),
            BanksClientError::TransactionError(err) => Self::other(err.to_string()),
            BanksClientError::SimulationError { err, .. } => Self::other(err.to_string()),
        }
    }
}

impl From<BanksClientError> for TransportError {
    fn from(err: BanksClientError) -> Self {
        match err {
            BanksClientError::ClientError(err) => Self::IoError(io::Error::other(err.to_string())),
            BanksClientError::Io(err) => Self::IoError(io::Error::other(err.to_string())),
            BanksClientError::RpcError(err) => Self::IoError(io::Error::other(err.to_string())),
            BanksClientError::TransactionError(err) => Self::TransactionError(err),
            BanksClientError::SimulationError { err, .. } => Self::TransactionError(err),
        }
    }
}
