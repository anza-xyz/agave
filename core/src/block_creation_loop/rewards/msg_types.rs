use {
    agave_votor_messages::reward_certificate::{NotarRewardCertificate, SkipRewardCertificate},
    crossbeam_channel::Sender,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::time::Instant,
};

/// Request to build reward certificates.
pub(crate) struct RewardRequest {
    /// First slot in the leader window for which to build certs.
    pub(crate) window_start: Slot,
    /// The channel on which to send the reply.
    pub(super) reply_sender: Sender<RewardResponse>,
    pub(super) request_sent: Instant,
}

/// Response when the reward certs are built successfully.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RewardRespSucc {
    /// Skip reward certificate.  None if no skip votes were registered.
    pub(crate) skip: Option<SkipRewardCertificate>,
    /// Notar reward certificate.  None if no notar votes were registered.
    pub(crate) notar: Option<NotarRewardCertificate>,
    /// Validators present in the certificates.
    pub(crate) validators: Vec<Pubkey>,
}

/// Response to a [`RewardRequest`].
pub(crate) struct RewardResponse {
    /// Bank slot for which reward certificates were requested.
    pub(crate) bank_slot: Slot,
    /// The result of building reward certs for `bank_slot`.
    pub(crate) result: Option<RewardRespSucc>,
}
