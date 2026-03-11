use thiserror::Error;

/// Errors that the [`SigVerifier`] can experience.
#[derive(Error, Debug)]
pub(super) enum SigVerifyError {
    #[error("verifying votes failed with {0}")]
    SigverifyVotes(#[from] SigVerifyVoteError),
    #[error("verifying certs failed with {0}")]
    SigverifyCerts(#[from] SigVerifyCertError),
}

/// Different types of errors that sig verifying votes can fail with.
#[derive(Debug, Error)]
#[allow(clippy::enum_variant_names)]
pub(super) enum SigVerifyVoteError {
    #[error("channel to consensus pool disconnected")]
    ConsensusPoolChannelDisconnected,
    #[error("channel to rewards container disconnected")]
    RewardsChannelDisconnected,
    #[error("channel to repair disconnected")]
    RepairChannelDisconnected,
    #[error("channel to metrics disconnected")]
    MetricsChannelDisconnected,
}

/// Different types of errors that sig verifying certs can fail with.
#[derive(Debug, Error)]
pub(super) enum SigVerifyCertError {
    #[error("channel to consensus pool disconnected")]
    ConsensusPoolChannelDisconnected,
}
