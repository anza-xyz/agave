use {
    solana_pubsub_client::nonblocking::pubsub_client::PubsubClientError,
    solana_rpc_client_api::client_error::Error as ClientError, thiserror::Error,
};

#[derive(Debug, Error)]
pub enum NodeAddressServiceError {
    #[error(transparent)]
    PubsubError(#[from] PubsubClientError),

    #[error(transparent)]
    RpcError(#[from] ClientError),

    #[error("Failed to get slot leaders connecting to: {0}")]
    SlotLeadersConnectionFailed(String),

    #[error("Failed find any cluster node info for upcoming leaders, timeout: {0}")]
    ClusterNodeNotFound(String),

    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),

    #[error("Unexpectly dropped a channel.")]
    ChannelClosed,

    #[error("Failed to initialize NodeAddressService.")]
    InitializationFailed,
}
