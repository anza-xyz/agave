pub use solana_rpc_response::*;
pub type RpcResult<T> = crate::client_error::Result<Response<T>>;
