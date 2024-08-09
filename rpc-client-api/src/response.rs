#[deprecated(since = "2.1.0", note = "Use solana-rpc-response crate instead")]
pub use solana_rpc_response::*;
pub type RpcResult<T> = crate::client_error::Result<Response<T>>;
