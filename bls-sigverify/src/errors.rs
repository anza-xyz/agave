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
    #[error("channel \"{0}\" disconnected")]
    ChannelDisconnected(&'static str),
}

/// Different types of errors that sig verifying certs can fail with.
#[derive(Debug, Error)]
pub(super) enum SigVerifyCertError {
    #[error("channel \"{0}\" disconnected")]
    ChannelDisconnected(&'static str),
}
