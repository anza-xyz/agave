use {
    solana_transaction_context::TransactionReturnData,
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

    pub fn transaction_error(&self) -> Option<&TransactionError> {
        match self {
            BanksClientError::TransactionError(err)
            | BanksClientError::SimulationError { err, .. } => Some(err),
            _ => None,
        }
    }
    pub fn simulation_logs(&self) -> Option<&[String]> {
        match self {
            BanksClientError::SimulationError { logs, .. } => Some(logs.as_slice()),
            _ => None,
        }
    }
    pub fn simulation_units_consumed(&self) -> Option<u64> {
        match self {
            BanksClientError::SimulationError { units_consumed, .. } => Some(*units_consumed),
            _ => None,
        }
    }
    pub fn simulation_return_data(&self) -> Option<&TransactionReturnData> {
        match self {
            BanksClientError::SimulationError { return_data: Some(d), .. } => Some(d),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_transaction::InstructionError;

    #[test]
    fn test_transaction_error_accessor() {
        let err = BanksClientError::TransactionError(TransactionError::AccountNotFound);
        assert!(matches!(err.transaction_error(), Some(TransactionError::AccountNotFound)));
        assert!(err.simulation_logs().is_none());
        assert!(err.simulation_units_consumed().is_none());
        assert!(err.simulation_return_data().is_none());
    }

    #[test]
    fn test_simulation_accessors() {
        let logs = vec!["log1".to_string(), "log2".to_string()];
        let return_data = TransactionReturnData {
            program_id: solana_pubkey::Pubkey::new_unique(),
            data: vec![1, 2, 3],
        };
        let err = BanksClientError::SimulationError {
            err: TransactionError::InstructionError(0, InstructionError::Custom(1)),
            logs: logs.clone(),
            units_consumed: 123,
            return_data: Some(return_data.clone()),
        };
        assert!(matches!(err.transaction_error(), Some(TransactionError::InstructionError(0, _))));
        assert_eq!(err.simulation_logs().unwrap(), logs.as_slice());
        assert_eq!(err.simulation_units_consumed(), Some(123));
        assert_eq!(err.simulation_return_data().unwrap().data, return_data.data);
        assert_eq!(
            err.simulation_return_data().unwrap().program_id,
            return_data.program_id
        );
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
            BanksClientError::ClientError(err) => Self::IoError(io::Error::other(err)),
            BanksClientError::Io(err) => Self::IoError(err),
            BanksClientError::RpcError(err) => Self::IoError(io::Error::other(err.to_string())),
            BanksClientError::TransactionError(err) => Self::TransactionError(err),
            BanksClientError::SimulationError { err, .. } => Self::TransactionError(err),
        }
    }
}
