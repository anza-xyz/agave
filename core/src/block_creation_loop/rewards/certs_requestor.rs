use {
    crate::block_creation_loop::rewards::msg_types::{
        RewardRequest, RewardRespSucc, RewardResponse,
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_runtime::leader_schedule_utils::{
        last_of_consecutive_leader_slots, remaining_slots_in_window,
    },
    std::{cmp::Ordering, time::Instant},
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
        my_pubkey: &Pubkey,
        window_start: Slot,
    ) -> Result<Option<RewardRequestToken>, ()> {
        let remaining_slots = remaining_slots_in_window(window_start);
        let (reply_sender, reply_receiver) = bounded(remaining_slots);
        let request = RewardRequest {
            window_start,
            reply_sender,
            request_sent: Instant::now(),
        };
        match self.req_sender.try_send(request) {
            Ok(()) => Ok(Some(RewardRequestToken::new(reply_receiver, window_start))),
            Err(TrySendError::Full(_)) => {
                warn!(
                    "{my_pubkey} sending request to build reward cert for \
                     bank_slot={window_start} failed, channel is full"
                );
                Ok(None)
            }
            Err(TrySendError::Disconnected(_)) => Err(()),
        }
    }
}

/// The request token to be used to receive responses on previously sent requests to build reward certs.
pub(crate) struct RewardRequestToken {
    reply_receiver: Option<Receiver<RewardResponse>>,
    pub(crate) window_start: Slot,
    current_slot: Slot,
}

impl RewardRequestToken {
    fn new(reply_receiver: Receiver<RewardResponse>, window_start: Slot) -> Self {
        Self {
            reply_receiver: Some(reply_receiver),
            window_start,
            current_slot: window_start,
        }
    }
    pub(crate) fn recv_next_cert(
        &mut self,
        my_pubkey: &Pubkey,
        bank_slot: Slot,
    ) -> Result<Option<RewardRespSucc>, ()> {
        let window_end = last_of_consecutive_leader_slots(self.window_start);
        if bank_slot < self.current_slot || bank_slot > window_end {
            return Ok(None);
        }
        let Some(receiver) = self.reply_receiver.as_ref() else {
            return Ok(None);
        };
        let result = loop {
            match receiver.try_recv() {
                Err(TryRecvError::Empty) => {
                    warn!("{my_pubkey} trying to receive cert failed, channel is empty");
                    break None;
                }
                Err(TryRecvError::Disconnected) => return Err(()),
                Ok(resp) => match resp.bank_slot.cmp(&bank_slot) {
                    Ordering::Equal => break resp.result,
                    Ordering::Less => (),
                    Ordering::Greater => {
                        error!(
                            "{my_pubkey}: fell out of sync: current_slot:{bank_slot}, \
                             received_slot:{}",
                            resp.bank_slot
                        );
                        break None;
                    }
                },
            }
        };
        self.current_slot = bank_slot + 1;
        if bank_slot == window_end {
            self.reply_receiver = None;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {

    use {
        super::*, crate::block_creation_loop::rewards::msg_types::RewardResponse,
        solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    };

    #[test]
    fn test_does_not_block() {
        let my_pubkey = Pubkey::default();
        let window_start = 4;
        let (mut handler, _receiver) = CertsRequestor::new();
        let mut token = handler
            .request_reward_certs(&my_pubkey, window_start)
            .unwrap()
            .unwrap();

        let resp = token.recv_next_cert(&my_pubkey, window_start).unwrap();
        assert_eq!(resp, None);

        {
            let (_reply_sender, reply_receiver) = bounded(0);
            let mut token = RewardRequestToken::new(reply_receiver, window_start);
            let resp = token.recv_next_cert(&my_pubkey, window_start).unwrap();
            assert_eq!(resp, None);
        }
    }

    #[test]
    fn test_delayed_none_response() {
        let my_pubkey = Pubkey::default();
        let window_start = 4;
        let remaining_slots = remaining_slots_in_window(window_start);
        let (reply_sender, reply_receiver) = bounded(remaining_slots);
        let mut token = RewardRequestToken::new(reply_receiver, window_start);

        // The first response arrives after its slot has already completed.
        assert_eq!(token.recv_next_cert(&my_pubkey, window_start), Ok(None));
        reply_sender
            .send(RewardResponse {
                bank_slot: window_start,
                result: None,
            })
            .unwrap();
        let validators = vec![Pubkey::new_unique()];
        reply_sender
            .send(RewardResponse {
                bank_slot: window_start + 1,
                result: Some(RewardRespSucc {
                    skip: None,
                    notar: None,
                    validators: validators.clone(),
                }),
            })
            .unwrap();

        let resp = token
            .recv_next_cert(&my_pubkey, window_start + 1)
            .unwrap()
            .unwrap();
        assert_eq!(resp.validators, validators);

        // A matching None response must also leave the next slot usable.
        reply_sender
            .send(RewardResponse {
                bank_slot: window_start + 2,
                result: None,
            })
            .unwrap();
        reply_sender
            .send(RewardResponse {
                bank_slot: window_start + 3,
                result: Some(resp),
            })
            .unwrap();
        assert_eq!(token.recv_next_cert(&my_pubkey, window_start + 2), Ok(None));
        assert_eq!(
            token
                .recv_next_cert(&my_pubkey, window_start + 3)
                .unwrap()
                .unwrap()
                .validators,
            validators
        );
        assert!(token.reply_receiver.is_none());
    }

    #[test]
    fn test_basic_functionality() {
        let my_pubkey = Pubkey::default();
        let window_start = 4;
        let (mut handler, receiver) = CertsRequestor::new();
        let mut token = handler
            .request_reward_certs(&my_pubkey, window_start)
            .unwrap()
            .unwrap();
        let RewardRequest {
            window_start: requested_window_start,
            reply_sender,
            request_sent: _,
        } = receiver.recv().unwrap();
        assert_eq!(requested_window_start, window_start);
        let validators_by_slot: Vec<_> = (0..NUM_CONSECUTIVE_LEADER_SLOTS.get())
            .map(|_| vec![Pubkey::new_unique(); 10])
            .collect();
        for (offset, validators) in validators_by_slot.iter().enumerate() {
            reply_sender
                .send(RewardResponse {
                    bank_slot: window_start + offset as Slot,
                    result: Some(RewardRespSucc {
                        skip: None,
                        notar: None,
                        validators: validators.clone(),
                    }),
                })
                .unwrap();
        }
        for (offset, validators) in validators_by_slot.iter().enumerate() {
            let resp = token
                .recv_next_cert(&my_pubkey, window_start + offset as Slot)
                .unwrap();
            assert!(resp.as_ref().unwrap().skip.is_none());
            assert!(resp.as_ref().unwrap().notar.is_none());
            assert_eq!(&resp.unwrap().validators, validators);
        }
        assert!(token.reply_receiver.is_none());
    }
}
