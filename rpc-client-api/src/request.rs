pub use solana_rpc_request::{
    RpcRequest, TokenAccountsFilter, DELINQUENT_VALIDATOR_SLOT_DISTANCE,
    MAX_GET_CONFIRMED_BLOCKS_RANGE, MAX_GET_CONFIRMED_SIGNATURES_FOR_ADDRESS2_LIMIT,
    MAX_GET_CONFIRMED_SIGNATURES_FOR_ADDRESS_SLOT_RANGE, MAX_GET_PROGRAM_ACCOUNT_FILTERS,
    MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS, MAX_GET_SLOT_LEADERS, MAX_MULTIPLE_ACCOUNTS,
    MAX_RPC_VOTE_ACCOUNT_INFO_EPOCH_CREDITS_HISTORY, NUM_LARGEST_ACCOUNTS,
};
use {
    crate::response::RpcSimulateTransactionResult, solana_clock::Slot, std::fmt, thiserror::Error,
};

#[derive(Debug)]
pub enum RpcResponseErrorData {
    Empty,
    SendTransactionPreflightFailure(RpcSimulateTransactionResult),
    NodeUnhealthy { num_slots_behind: Option<Slot> },
}

impl fmt::Display for RpcResponseErrorData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RpcResponseErrorData::SendTransactionPreflightFailure(
                RpcSimulateTransactionResult {
                    logs: Some(logs), ..
                },
            ) => {
                if logs.is_empty() {
                    Ok(())
                } else {
                    writeln!(f, "{} log messages:", logs.len())?;
                    for log in logs {
                        writeln!(f, "  {log}")?;
                    }
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("RPC request error: {0}")]
    RpcRequestError(String),
    #[error("RPC response error {code}: {message}; {data}")]
    RpcResponseError {
        code: i64,
        message: String,
        data: RpcResponseErrorData,
    },
    #[error("parse error: expected {0}")]
    ParseError(String), /* "expected" */
    // Anything in a `ForUser` needs to die.  The caller should be
    // deciding what to tell their user
    #[error("{0}")]
    ForUser(String), /* "direct-to-user message" */
}
