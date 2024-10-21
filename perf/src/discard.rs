use {
    crate::mutable_packet_batch::MutablePacketBatch,
    rand::{thread_rng, Rng},
};

pub fn discard_batches_randomly(
    batches: &mut Vec<impl MutablePacketBatch>,
    max_packets: usize,
    mut total_packets: usize,
) -> usize {
    while total_packets > max_packets {
        let index = thread_rng().gen_range(0..batches.len());
        let removed = batches.swap_remove(index);
        total_packets = total_packets.saturating_sub(removed.borrow().len());
    }
    total_packets
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::packet::{Packet, PacketBatch},
    };

    #[test]
    fn test_batch_discard_random() {
        solana_logger::setup();
        let mut batch = PacketBatch::default();
        batch.resize(1, Packet::default());
        let num_batches = 100;
        let mut batches = vec![batch; num_batches];
        let max = 5;
        discard_batches_randomly(&mut batches, max, num_batches);
        assert_eq!(batches.len(), max);
    }
}
