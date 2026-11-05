use {
    crate::block_creation_loop::{
        SlotMetrics,
        rewards::msg_types::{RewardRequest, RewardRequestToken, RewardRespSucc},
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded},
    solana_clock::Slot,
    solana_measure::measure::Measure,
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

    /// Sends a message requesting a reward cert to be built.
    pub(crate) fn request_reward_certs(
        &self,
        stats: &mut SlotMetrics,
        my_pubkey: Pubkey,
        bank_slot: Slot,
    ) -> Result<RewardRequestToken, ()> {
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
                     failed, channel is full"
                );
                let measure = Measure::start("blocking send");
                if self.req_sender.send(request).is_err() {
                    return Err(());
                }
                stats
                    .reward_certs_blocked_us
                    .add_sample(measure.end_as_us());
            }
            Err(TrySendError::Disconnected(_)) => return Err(()),
        }
        Ok(RewardRequestToken {
            reply_receiver,
            bank_slot,
        })
    }

    /// Receives a response for an earlier request to build reward certs.
    pub(crate) fn recv_reward_certs(
        &self,
        stats: &mut SlotMetrics,
        my_pubkey: Pubkey,
        token: RewardRequestToken,
    ) -> Result<RewardRespSucc, ()> {
        let resp = match token.reply_receiver.try_recv() {
            Ok(resp) => resp,
            Err(TryRecvError::Disconnected) => return Err(()),
            Err(TryRecvError::Empty) => {
                warn!(
                    "{my_pubkey}: blocking wait for reward response in bank_slot={}",
                    token.bank_slot
                );
                let measure = Measure::start("blocking recv");
                let Ok(resp) = token.reply_receiver.recv() else {
                    return Err(());
                };
                stats
                    .reward_certs_blocked_us
                    .add_sample(measure.end_as_us());
                resp
            }
        };
        match resp.result {
            Ok(res) => Ok(res),
            Err(err) => {
                error!("{my_pubkey} building reward cert failed with {err:?}");
                Ok(RewardRespSucc::default())
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
            .unwrap();
        assert!(resp.skip.is_none());
        assert!(resp.notar.is_none());
        assert_eq!(resp.validators, validators);
    }
}
