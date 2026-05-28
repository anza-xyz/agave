use {
    agave_votor::consensus_rewards::{
        BuildRewardCertsRequest, BuildRewardCertsRespSucc, BuildRewardCertsResponse,
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded},
    solana_clock::Slot,
    solana_poh::poh_recorder::PohRecorderError,
    solana_pubkey::Pubkey,
};

/// The request token to be used to receive responses on previously sent requests to build reward certs.
pub(super) struct RewardCertRequest {
    reply_receiver: Receiver<BuildRewardCertsResponse>,
}

/// Struct to handle requests to build rewards certs and receive the produced certs.
pub(crate) struct RewardCertsHandler {
    sender: Sender<BuildRewardCertsRequest>,
}

impl RewardCertsHandler {
    pub(crate) fn new() -> (Self, Receiver<BuildRewardCertsRequest>) {
        // If the rewards container is keeping up, we should only ever expect there to be 1 msg in
        // flight.  So using a fairly small capacity to allow the rewards container to catch up if
        // it ever falls behind.
        const CAPACITY: usize = 16;

        let (tx, rx) = bounded(CAPACITY);
        (Self { sender: tx }, rx)
    }

    /// Tries to send a message requesting a reward cert to be built.
    ///
    /// Does not block trying to send a message as we have a tight deadline to produce the block
    /// and if the rewards container is not keeping up, then we would rather not include a rewards
    /// cert than wait.
    ///
    /// Returns:
    /// - `Ok(Some(RewardCertRequest))`` if sending succeeded.  The `RewardCertRequest` can be
    ///   used to receive a response to the build the reward cert.
    /// - `Ok(None)` if sending failed because the channel is full i.e. the rewards container is
    ///   not keeping up.
    /// - `Err(PohRecorderError::ChannelDisconnected)` if the channel is to the rewards container
    ///   is disconnected.
    pub(super) fn request_reward_certs(
        &mut self,
        my_pubkey: Pubkey,
        bank_slot: Slot,
    ) -> Result<Option<RewardCertRequest>, PohRecorderError> {
        let (reply_sender, reply_receiver) = bounded(1);
        let request = BuildRewardCertsRequest {
            bank_slot,
            reply_sender,
        };
        match self.sender.try_send(request) {
            Ok(()) => Ok(Some(RewardCertRequest { reply_receiver })),
            Err(TrySendError::Full(_)) => {
                warn!(
                    "{my_pubkey} sending request to build reward cert for bank_slot={bank_slot} \
                     failed, channel is full"
                );
                Ok(None)
            }
            Err(TrySendError::Disconnected(_)) => Err(PohRecorderError::ChannelDisconnected),
        }
    }

    /// Tries to receive a response for an earlier request to build reward certs.
    ///
    /// Does not block trying to receive the response as we have a tight deadline to produce
    /// the block and if the rewards container is not keeping up, we will rather not include any
    /// reward certs than wait for it.
    pub(super) fn recv_reward_certs(
        &mut self,
        my_pubkey: Pubkey,
        request: Option<RewardCertRequest>,
    ) -> Result<BuildRewardCertsRespSucc, PohRecorderError> {
        match request {
            None => Ok(BuildRewardCertsRespSucc::default()),
            Some(request) => {
                let msg = request.reply_receiver.try_recv();
                match msg {
                    Err(TryRecvError::Empty) => {
                        warn!("{my_pubkey} trying to receive cert failed, channel is empty");
                        Ok(BuildRewardCertsRespSucc::default())
                    }
                    Err(TryRecvError::Disconnected) => Err(PohRecorderError::ChannelDisconnected),
                    Ok(resp) => match resp.result {
                        Ok(res) => Ok(res),
                        Err(err) => {
                            error!("{my_pubkey} building reward cert failed with {err:?}");
                            Ok(BuildRewardCertsRespSucc::default())
                        }
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_does_not_block() {
        let my_pubkey = Pubkey::default();
        let (mut handler, _receiver) = RewardCertsHandler::new();
        handler.request_reward_certs(my_pubkey, 1).unwrap().unwrap();

        let resp = handler.recv_reward_certs(my_pubkey, None).unwrap();
        assert_eq!(resp, BuildRewardCertsRespSucc::default());

        {
            let (_reply_sender, reply_receiver) = bounded(0);
            let request = RewardCertRequest { reply_receiver };
            let resp = handler.recv_reward_certs(my_pubkey, Some(request)).unwrap();
            assert_eq!(resp, BuildRewardCertsRespSucc::default());
        }
    }

    #[test]
    fn test_basic_functionality() {
        let my_pubkey = Pubkey::default();
        let (mut handler, receiver) = RewardCertsHandler::new();
        let request = handler.request_reward_certs(my_pubkey, 1).unwrap().unwrap();
        let validators = vec![Pubkey::new_unique(); 10];

        let BuildRewardCertsRequest {
            bank_slot: _,
            reply_sender,
        } = receiver.recv().unwrap();
        reply_sender
            .send(BuildRewardCertsResponse {
                result: Ok(BuildRewardCertsRespSucc {
                    skip: None,
                    notar: None,
                    validators: validators.clone(),
                }),
            })
            .unwrap();
        let resp = handler.recv_reward_certs(my_pubkey, Some(request)).unwrap();
        assert!(resp.skip.is_none());
        assert!(resp.notar.is_none());
        assert_eq!(resp.validators, validators);
    }
}
