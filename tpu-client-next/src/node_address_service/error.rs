//! Error for [`NodeAddressService`].
use {
    crate::node_address_service::{
        leader_tpu_cache_service::Error as LeaderTpuCacheServiceError,
        slot_receiver::SlotReceiverError,
        websocket_slot_update_service::WebsocketSlotUpdateServiceError,
    },
    solana_rpc_client_api::client_error::Error as ClientError,
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum NodeAddressServiceError {
    #[error(transparent)]
    RpcError(#[from] ClientError),

    #[error(transparent)]
    SlotReceiverError(#[from] SlotReceiverError),

    #[error(transparent)]
    WebsocketSlotUpdateServiceError(#[from] WebsocketSlotUpdateServiceError),

    #[error(transparent)]
    LeaderTpuCacheServiceError(#[from] LeaderTpuCacheServiceError),
}
