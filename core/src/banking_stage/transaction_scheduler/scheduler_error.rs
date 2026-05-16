use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("Sending channel disconnected: {0}")]
    DisconnectedSendChannel(&'static str),
    #[error("Recv channel disconnected: {0}")]
    DisconnectedRecvChannel(&'static str),
    #[error("Tip processor failed for slot {slot}: {reason}")]
    TipProcessorFailed { slot: u64, reason: String },
}
