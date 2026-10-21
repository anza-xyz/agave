use {
    agave_votor_messages::reward_certificate::{NotarRewardCertificate, SkipRewardCertificate},
    solana_pubkey::Pubkey,
};

/// Response when the reward certs are built successfully.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct RewardRespSucc {
    /// Skip reward certificate.  None if no skip votes were registered.
    pub(crate) skip: Option<SkipRewardCertificate>,
    /// Notar reward certificate.  None if no notar votes were registered.
    pub(crate) notar: Option<NotarRewardCertificate>,
    /// If at least one of the certs above is present, then this contains the slot for which the
    /// reward certs were built and the list of validators in the certs.
    pub(crate) validators: Vec<Pubkey>,
}
