use thiserror::Error;

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
