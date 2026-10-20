use {
    crate::block_creation_loop::{
        SlotMetrics,
        rewards::msg_types::{RewardRequest, RewardRequestToken, RewardRespSucc},
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::time::Instant,
};

/// Struct to handle requests to build rewards certs and receive the produced certs.
pub(crate) struct CertsRequestor {
    req_sender: Sender<RewardRequest>,
}

impl CertsRequestor {
    pub(crate) fn new() -> (Self, Receiver<RewardRequest>) {
        // If the rewards container is keeping up, we should only ever expect there to be 1 msg in
        // flight.  So using a fairly small capacity to allow the rewards container to catch up if
        // it ever falls behind.
        const CAPACITY: usize = 16;

        let (tx, rx) = bounded(CAPACITY);
        (Self { req_sender: tx }, rx)
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
    /// - `Err(())` if the channel to the rewards container is disconnected.
    pub(crate) fn request_reward_certs(
        &mut self,
        stats: &mut SlotMetrics,
        my_pubkey: Pubkey,
        bank_slot: Slot,
    ) -> Result<Option<RewardRequestToken>, ()> {
        let (reply_sender, reply_receiver) = bounded(1);
        let request_sent = Instant::now();
        let request = RewardRequest {
            bank_slot,
            reply_sender,
            request_sent,
        };
        match self.req_sender.try_send(request) {
            Ok(()) => Ok(Some(RewardRequestToken {
                reply_receiver,
                request_sent,
            })),
            Err(TrySendError::Full(_)) => {
                warn!(
                    "{my_pubkey} sending request to build reward cert for bank_slot={bank_slot} \
                     failed, channel is full"
                );
                stats.reward_certs_skipped += 1;
                Ok(None)
            }
            Err(TrySendError::Disconnected(_)) => Err(()),
        }
    }

    /// Tries to receive a response for an earlier request to build reward certs.
    ///
    /// Does not block trying to receive the response as we have a tight deadline to produce
    /// the block and if the rewards container is not keeping up, we will rather not include any
    /// reward certs than wait for it. Once the direct snapshot is available, use an optional
    /// recovery upgrade only if it has already completed; an empty or closed recovery channel
    /// always leaves the direct snapshot available.
    pub(crate) fn recv_reward_certs(
        &mut self,
        stats: &mut SlotMetrics,
        my_pubkey: Pubkey,
        request: Option<RewardRequestToken>,
    ) -> Result<RewardRespSucc, ()> {
        match request {
            None => Ok(RewardRespSucc::default()),
            Some(request) => {
                stats.reward_certs_observation_us = Some(
                    request
                        .request_sent
                        .elapsed()
                        .as_micros()
                        .try_into()
                        .unwrap_or(u64::MAX),
                );
                let msg = request.reply_receiver.try_recv();
                match msg {
                    Err(TryRecvError::Empty) => {
                        warn!("{my_pubkey} trying to receive cert failed, channel is empty");
                        stats.reward_certs_skipped += 1;
                        Ok(RewardRespSucc::default())
                    }
                    Err(TryRecvError::Disconnected) => Err(()),
                    Ok(resp) => match resp.result {
                        Ok(baseline) => match resp.recovery.map(|receiver| receiver.try_recv()) {
                            Some(Ok(upgrade)) => {
                                stats.reward_certs_recovery_ready += 1;
                                Ok(upgrade)
                            }
                            Some(Err(TryRecvError::Empty)) => {
                                stats.reward_certs_recovery_pending += 1;
                                Ok(baseline)
                            }
                            Some(Err(TryRecvError::Disconnected)) | None => Ok(baseline),
                        },
                        Err(err) => {
                            error!("{my_pubkey} building reward cert failed with {err:?}");
                            Ok(RewardRespSucc::default())
                        }
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use {super::*, crate::block_creation_loop::rewards::msg_types::RewardResponse};

    #[test]
    fn test_does_not_block() {
        let mut slot_metrics = SlotMetrics::new(1, true);
        let my_pubkey = Pubkey::default();
        let (mut handler, _receiver) = CertsRequestor::new();
        handler
            .request_reward_certs(&mut slot_metrics, my_pubkey, 1)
            .unwrap()
            .unwrap();

        let resp = handler
            .recv_reward_certs(&mut slot_metrics, my_pubkey, None)
            .unwrap();
        assert_eq!(resp, RewardRespSucc::default());

        {
            let (_reply_sender, reply_receiver) = bounded(0);
            let request = RewardRequestToken {
                reply_receiver,
                request_sent: Instant::now(),
            };
            let resp = handler
                .recv_reward_certs(&mut slot_metrics, my_pubkey, Some(request))
                .unwrap();
            assert_eq!(resp, RewardRespSucc::default());
        }
    }

    #[test]
    fn test_basic_functionality() {
        let mut slot_metrics = SlotMetrics::new(1, true);
        let my_pubkey = Pubkey::default();
        let (mut handler, receiver) = CertsRequestor::new();
        let request = handler
            .request_reward_certs(&mut slot_metrics, my_pubkey, 1)
            .unwrap()
            .unwrap();
        let validators = vec![Pubkey::new_unique(); 10];

        let RewardRequest {
            bank_slot: _,
            reply_sender,
            request_sent: _,
        } = receiver.recv().unwrap();
        reply_sender
            .send(RewardResponse {
                result: Ok(RewardRespSucc {
                    skip: None,
                    notar: None,
                    validators: validators.clone(),
                }),
                recovery: None,
            })
            .unwrap();
        let resp = handler
            .recv_reward_certs(&mut slot_metrics, my_pubkey, Some(request))
            .unwrap();
        assert!(resp.skip.is_none());
        assert!(resp.notar.is_none());
        assert_eq!(resp.validators, validators);
    }

    #[test]
    fn test_recovery_uses_only_an_already_completed_upgrade() {
        for completed in [false, true] {
            let (mut requestor, _requests) = CertsRequestor::new();
            let mut metrics = SlotMetrics::new(100, true);
            let baseline = Pubkey::new_unique();
            let recovered = Pubkey::new_unique();
            let (upgrade, recovery) = bounded(1);
            if completed {
                upgrade
                    .send(RewardRespSucc {
                        validators: vec![baseline, recovered],
                        ..RewardRespSucc::default()
                    })
                    .unwrap();
            }
            let (reply, reply_receiver) = bounded(1);
            reply
                .send(RewardResponse {
                    result: Ok(RewardRespSucc {
                        validators: vec![baseline],
                        ..RewardRespSucc::default()
                    }),
                    recovery: Some(recovery),
                })
                .unwrap();
            let response = requestor
                .recv_reward_certs(
                    &mut metrics,
                    Pubkey::new_unique(),
                    Some(RewardRequestToken {
                        reply_receiver,
                        request_sent: Instant::now(),
                    }),
                )
                .unwrap();
            assert_eq!(metrics.reward_certs_skipped, 0);
            assert_eq!(
                response.validators,
                if completed {
                    vec![baseline, recovered]
                } else {
                    vec![baseline]
                }
            );
            // The cutoff consumes the token. Later optional output cannot replace its snapshot.
            assert!(upgrade.try_send(RewardRespSucc::default()).is_err());
        }
    }

    #[test]
    fn test_disconnected_recovery_preserves_direct_snapshot() {
        let (mut requestor, _requests) = CertsRequestor::new();
        let mut metrics = SlotMetrics::new(100, true);
        let baseline = Pubkey::new_unique();
        let (upgrade, recovery) = bounded(1);
        drop(upgrade);
        let (reply, reply_receiver) = bounded(1);
        reply
            .send(RewardResponse {
                result: Ok(RewardRespSucc {
                    validators: vec![baseline],
                    ..RewardRespSucc::default()
                }),
                recovery: Some(recovery),
            })
            .unwrap();
        let response = requestor
            .recv_reward_certs(
                &mut metrics,
                Pubkey::new_unique(),
                Some(RewardRequestToken {
                    reply_receiver,
                    request_sent: Instant::now(),
                }),
            )
            .unwrap();
        assert_eq!(response.validators, vec![baseline]);
        assert_eq!(metrics.reward_certs_skipped, 0);
    }
}
