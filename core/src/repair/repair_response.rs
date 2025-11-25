use {
    solana_clock::Slot,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{Nonce, SIZE_OF_NONCE},
    },
    solana_packet::Packet,
    std::{io, net::SocketAddr},
};

pub fn repair_response_packet(
    blockstore: &Blockstore,
    slot: Slot,
    shred_index: u64,
    dest: &SocketAddr,
    nonce: Nonce,
) -> Option<Packet> {
    let shred = blockstore
        .get_data_shred(slot, shred_index)
        .expect("Blockstore could not get data shred");
    shred
        .map(|shred| repair_response_packet_from_bytes(shred, dest, nonce))
        .unwrap_or(None)
}

pub fn repair_response_packet_from_bytes(
    bytes: impl AsRef<[u8]>,
    dest: &SocketAddr,
    nonce: Nonce,
) -> Option<Packet> {
    let bytes = bytes.as_ref();
    let mut packet = Packet::default();
    let size = bytes.len() + SIZE_OF_NONCE;
    if size > packet.buffer_mut().len() {
        return None;
    }
    packet.meta_mut().size = size;
    packet.meta_mut().set_socket_addr(dest);
    packet.buffer_mut()[..bytes.len()].copy_from_slice(bytes);
    let mut wr = io::Cursor::new(&mut packet.buffer_mut()[bytes.len()..]);
    bincode::serialize_into(&mut wr, &nonce).expect("Buffer not large enough to fit nonce");
    Some(packet)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_keypair::Keypair,
        solana_ledger::{
            shred::Shredder,
            sigverify_shreds::{verify_shred_cpu, LruCache, SlotPubkeys},
        },
        solana_packet::PacketFlags,
        solana_signer::Signer,
        std::{
            collections::HashMap,
            net::{IpAddr, Ipv4Addr},
            sync::RwLock,
        },
    };

    fn run_test_sigverify_shred_cpu_repair(slot: Slot) {
        agave_logger::setup();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let keypair = Keypair::new();
        let shred = Shredder::single_shred_for_tests(slot, &keypair);

        trace!("signature {}", shred.signature());
        let nonce = 9;
        let mut packet = repair_response_packet_from_bytes(
            shred.into_payload(),
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            nonce,
        )
        .unwrap();
        packet.meta_mut().flags |= PacketFlags::REPAIR;

        let leader_slots: SlotPubkeys = [(slot, keypair.pubkey())].into_iter().collect();
        assert!(verify_shred_cpu((&packet).into(), &leader_slots, &cache));

        let wrong_keypair = Keypair::new();
        let leader_slots: SlotPubkeys = [(slot, wrong_keypair.pubkey())].into_iter().collect();
        assert!(!verify_shred_cpu((&packet).into(), &leader_slots, &cache));

        let leader_slots: SlotPubkeys = HashMap::default();
        assert!(!verify_shred_cpu((&packet).into(), &leader_slots, &cache));
    }

    #[test]
    fn test_sigverify_shred_cpu_repair() {
        run_test_sigverify_shred_cpu_repair(0xdead_c0de);
    }
}
