use {
    crate::block_creation_loop::{
        SlotMetrics,
        rewards::msg_types::{RewardRequest, RewardRequestToken, RewardRespSucc},
    },
    crossbeam_channel::{
        Receiver, RecvTimeoutError, SendTimeoutError, Sender, TryRecvError, TrySendError, bounded,
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    std::time::{Duration, Instant},
};

const MAX_WAIT_TIME: Duration = Duration::from_millis(100);

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

    /// Sends a message requesting a reward cert to be built.
    ///
    /// If the channel is full, waits up to `MAX_WAIT_TIME` for it to become available.
    pub(crate) fn request_reward_certs(
        &self,
        stats: &mut SlotMetrics,
        my_pubkey: Pubkey,
        bank_slot: Slot,
    ) -> Result<Option<RewardRequestToken>, ()> {
        let (reply_sender, reply_receiver) = bounded(1);
        let request = RewardRequest {
            bank_slot,
            reply_sender,
            request_sent: Instant::now(),
        };
        match self.req_sender.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) => {
                warn!(
                    "{my_pubkey} sending request to build reward cert for bank_slot={bank_slot} \
                     failed, channel is full. waiting until channel has space."
                );
                let measure = Measure::start("blocking send");
                let res = self.req_sender.send_timeout(request, MAX_WAIT_TIME);
                stats
                    .reward_certs_blocked_us
                    .add_sample(measure.end_as_us());
                match res {
                    Ok(()) => {}
                    Err(SendTimeoutError::Disconnected(_)) => return Err(()),
                    Err(SendTimeoutError::Timeout(_)) => {
                        stats.reward_certs_skipped += 1;
                        warn!(
                            "{my_pubkey}: sending request to build reward certs for \
                             bank_slot={bank_slot} took too long. skipping."
                        );
                        return Ok(None);
                    }
                }
            }
            Err(TrySendError::Disconnected(_)) => return Err(()),
        }
        Ok(Some(RewardRequestToken {
            reply_receiver,
            bank_slot,
        }))
    }

    /// Receives a response for an earlier request to build reward certs.
    ///
    /// If the cert is not ready, waits up to `MAX_WAIT_TIME` for it to become ready.
    pub(crate) fn recv_reward_certs(
        &self,
        stats: &mut SlotMetrics,
        my_pubkey: Pubkey,
        token: Option<RewardRequestToken>,
    ) -> Result<Option<RewardRespSucc>, ()> {
        let Some(token) = token else {
            return Ok(None);
        };
        let resp = match token.reply_receiver.try_recv() {
            Ok(resp) => resp,
            Err(TryRecvError::Disconnected) => return Err(()),
            Err(TryRecvError::Empty) => {
                warn!(
                    "{my_pubkey}: reward certificate in bank_slot={} not ready yet. blocking",
                    token.bank_slot
                );
                let measure = Measure::start("blocking recv");
                let res = token.reply_receiver.recv_timeout(MAX_WAIT_TIME);
                stats
                    .reward_certs_blocked_us
                    .add_sample(measure.end_as_us());
                match res {
                    Ok(resp) => resp,
                    Err(RecvTimeoutError::Timeout) => {
                        stats.reward_certs_skipped += 1;
                        warn!(
                            "{my_pubkey}: waiting too long for reward cert to become ready. \
                             skipping."
                        );
                        return Ok(None);
                    }
                    Err(RecvTimeoutError::Disconnected) => return Err(()),
                }
            }
        };
        match resp.result {
            Ok(res) => Ok(Some(res)),
            Err(err) => {
                error!("{my_pubkey} building reward cert failed with {err:?}");
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use {super::*, crate::block_creation_loop::rewards::msg_types::RewardResponse};

    #[test]
    fn test_basic_functionality() {
        let mut slot_metrics = SlotMetrics::new(1, true);
        let my_pubkey = Pubkey::default();
        let (handler, receiver) = CertsRequestor::new();
        let request = handler
            .request_reward_certs(&mut slot_metrics, my_pubkey, 1)
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
            })
            .unwrap();
        let resp = handler
            .recv_reward_certs(&mut slot_metrics, my_pubkey, request)
            .unwrap()
            .unwrap();
        assert!(resp.skip.is_none());
        assert!(resp.notar.is_none());
        assert_eq!(resp.validators, validators);
    }
}
