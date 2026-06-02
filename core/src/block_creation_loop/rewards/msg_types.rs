use {
    agave_votor_messages::{
        consensus_message::VoteMessage,
        reward_certificate::{
            BuildRewardCertsRespError, NotarRewardCertificate, SkipRewardCertificate,
        },
    },
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
};

/// Request to build reward certificates.
pub(crate) struct RewardRequest {
    /// The bank slot which will include the built reward certs.
    pub(crate) bank_slot: Slot,
    /// The channel on which to send the reply.
    pub(super) reply_sender: Sender<RewardResponse>,
}

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

/// Response to a [`RewardRequest`].
pub(crate) struct RewardResponse {
    /// The result of building reward certs for `bank_slot`.
    pub(crate) result: Result<RewardRespSucc, BuildRewardCertsRespError>,
}

/// Message to add votes to the rewards container.
#[derive(Debug)]
pub struct AddVoteMessage {
    /// List of [`VoteMessage`]s.
    pub(crate) votes: Vec<VoteMessage>,
}

/// The request token to be used to receive responses on previously sent requests to build reward certs.
pub(crate) struct RewardRequestToken {
    pub(super) reply_receiver: Receiver<RewardResponse>,
}
