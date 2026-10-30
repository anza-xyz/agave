use {
    agave_banking_stage_ingress_types::{BankingPacketBatch, BankingPacketReceiver},
    crossbeam_channel::{Receiver, SendError, TrySendError, bounded},
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
};

/// Capacity of the vote channel between sigverify and the banking-stage.
/// Sized to fit all votes from a reasonably sized cluster for 1 slot, + margin.
const VOTE_CHANNEL_CAPACITY: usize = 1024 * 8;

/// Capacity of the non-vote (transaction) channel between sigverify and the banking-stage.
/// Larger than the vote channel to absorb bursty TPU load.
const NON_VOTE_CHANNEL_CAPACITY: usize = 1024 * 16;

/// A small grouping struct to temporarily hold banking packet channel senders and receivers with
/// different source labels for the banking stage setup.
pub struct Channels {
    pub non_vote_sender: BankingPacketSender,
    pub non_vote_receiver: BankingPacketReceiver,
    pub tpu_vote_sender: BankingPacketSender,
    pub tpu_vote_receiver: BankingPacketReceiver,
    pub gossip_vote_sender: BankingPacketSender,
    pub gossip_vote_receiver: BankingPacketReceiver,
}

impl Channels {
    pub fn new() -> Self {
        let (non_vote_sender, non_vote_receiver) = channel(NON_VOTE_CHANNEL_CAPACITY);
        let (tpu_vote_sender, tpu_vote_receiver) = channel(VOTE_CHANNEL_CAPACITY);
        let (gossip_vote_sender, gossip_vote_receiver) = channel(VOTE_CHANNEL_CAPACITY);
        Self {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        }
    }
}

impl Default for Channels {
    fn default() -> Self {
        Self::new()
    }
}

pub fn channel_for_test() -> (BankingPacketSender, Receiver<BankingPacketBatch>) {
    channel(VOTE_CHANNEL_CAPACITY)
}

fn channel(capacity: usize) -> (BankingPacketSender, Receiver<BankingPacketBatch>) {
    let (sender, receiver) = bounded(capacity);
    let sender = EvictingSender::new(sender, receiver.clone());
    (BankingPacketSender { sender }, receiver)
}

/// Sender of banking packet batches, which evicts the oldest batch when the channel is full.
#[derive(Clone)]
pub struct BankingPacketSender {
    sender: EvictingSender<BankingPacketBatch>,
}

impl BankingPacketSender {
    /// Send a batch on the channel. This may evict an existing batch to make
    /// room; in that case `Ok(n)` is returned where `n` is the number of
    /// evicted packets. On channel disconnect returns `Err(SendError)`.
    pub fn send(&self, batch: BankingPacketBatch) -> Result<usize, SendError<BankingPacketBatch>> {
        match self.sender.try_send(batch) {
            Ok(()) => Ok(0),
            Err(TrySendError::Full(b)) => Ok(b.len()),
            Err(TrySendError::Disconnected(b)) => Err(SendError(b)),
        }
    }

    pub fn len(&self) -> usize {
        self.sender.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, agave_banking_stage_ingress_types::to_banking_packet_batch,
        solana_perf::test_tx::test_tx,
    };

    #[test]
    fn test_send_evicts_oldest_batch_when_full() {
        let (sender, receiver) = channel(1);
        let first = to_banking_packet_batch(&vec![test_tx(); 4]);
        let second = to_banking_packet_batch(&vec![test_tx(); 2]);

        assert_eq!(sender.send(first).unwrap(), 0);
        assert_eq!(sender.send(BankingPacketBatch::clone(&second)).unwrap(), 4);
        assert_eq!(sender.len(), 1);
        assert_eq!(receiver.try_recv().unwrap().len(), second.len());
        assert!(sender.is_empty());
    }
}
