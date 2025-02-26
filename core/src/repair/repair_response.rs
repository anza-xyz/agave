use {
    bytes::{BufMut, BytesMut},
    solana_ledger::{
        blockstore::Blockstore,
        shred::{Nonce, SIZE_OF_NONCE},
    },
    solana_sdk::{
        clock::Slot,
        packet::{Meta, Packet},
    },
    std::{io::Write, net::SocketAddr},
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
    bytes: Vec<u8>,
    dest: &SocketAddr,
    nonce: Nonce,
) -> Option<Packet> {
    // let bytes = bytes.as_ref();
    let mut meta = Meta::default();
    meta.set_socket_addr(dest);
    // NOTE(vadorovsky): It's a bummer that there is no (safe) way to convert
    // a `Vec<u8>` into `BytesMut`, even though `BytesMut` is built on top of
    // `Vec<u8>`. There is even an internal `BytesMut::from_vec`[0] method.
    //
    // For now, we make a copy here...
    //
    // [0] https://docs.rs/bytes/1.10.0/src/bytes/bytes_mut.rs.html#924
    let mut wr = BytesMut::with_capacity(bytes.len() + SIZE_OF_NONCE).writer();
    wr.write(&bytes)
        .expect("Buffer not large enough to fit the payload");
    bincode::serialize_into(&mut wr, &nonce).expect("Buffer not large enough to fit nonce");
    let buffer = wr.into_inner().freeze();
    let packet = Packet::new(buffer, meta);
    Some(packet)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_ledger::{
            shred::{Shred, ShredFlags},
            sigverify_shreds::{verify_shred_cpu, LruCache},
        },
        solana_sdk::{
            packet::PacketFlags,
            signature::{Keypair, Signer},
        },
        std::{
            collections::HashMap,
            net::{IpAddr, Ipv4Addr},
            sync::RwLock,
        },
    };

    fn run_test_sigverify_shred_cpu_repair(slot: Slot) {
        solana_logger::setup();
        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let mut shred = Shred::new_from_data(
            slot,
            0xc0de,
            0xdead,
            &[1, 2, 3, 4],
            ShredFlags::LAST_SHRED_IN_SLOT,
            0,
            0,
            0xc0de,
        );
        assert_eq!(shred.slot(), slot);
        let keypair = Keypair::new();
        shred.sign(&keypair);
        trace!("signature {}", shred.signature());
        let nonce = 9;
        let mut packet = repair_response_packet_from_bytes(
            shred.into_payload(),
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            nonce,
        )
        .unwrap();
        packet.meta_mut().flags |= PacketFlags::REPAIR;

        let leader_slots = HashMap::from([(slot, keypair.pubkey())]);
        assert!(verify_shred_cpu(&packet, &leader_slots, &cache));

        let wrong_keypair = Keypair::new();
        let leader_slots = HashMap::from([(slot, wrong_keypair.pubkey())]);
        assert!(!verify_shred_cpu(&packet, &leader_slots, &cache));

        let leader_slots = HashMap::new();
        assert!(!verify_shred_cpu(&packet, &leader_slots, &cache));
    }

    #[test]
    fn test_sigverify_shred_cpu_repair() {
        run_test_sigverify_shred_cpu_repair(0xdead_c0de);
    }
}
