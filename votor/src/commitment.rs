use thiserror::Error;

#[derive(Debug, Error)]
pub enum AlpenglowCommitmentError {
    #[error("Failed to send commitment data, channel disconnected")]
    ChannelDisconnected,
}
