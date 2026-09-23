use {
    bytes::{BufMut, BytesMut},
    solana_clock::Slot,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{Nonce, SIZE_OF_NONCE},
    },
    solana_packet::{Meta, PACKET_DATA_SIZE},
    solana_perf::packet::BytesPacket,
    std::net::SocketAddr,
};

pub fn repair_response_packet(
    blockstore: &Blockstore,
    slot: Slot,
    shred_index: u64,
    dest: &SocketAddr,
    nonce: Nonce,
) -> Option<BytesPacket> {
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
) -> Option<BytesPacket> {
    let bytes = bytes.as_ref();
    let size = bytes.len() + SIZE_OF_NONCE;
    if size > PACKET_DATA_SIZE {
        return None;
    }
    let mut buffer = BytesMut::with_capacity(size);
    buffer.put_slice(bytes);
    buffer.put_u32_le(nonce);
    let mut meta = Meta::default();
    meta.size = size;
    meta.set_socket_addr(dest);
    Some(BytesPacket::new(buffer.freeze(), meta))
}

#[cfg(test)]
mod test {
    use {
        super::*,
        bytes::Bytes,
        solana_keypair::Keypair,
        solana_ledger::shred::{AdmissionPolicy, Shredder, parse_repair},
        solana_signer::Signer,
        std::net::{IpAddr, Ipv4Addr},
    };

    fn run_test_sigverify_shred_repair(slot: Slot) {
        agave_logger::setup();
        let keypair = Keypair::new();
        let shred = Shredder::single_shred_for_tests(slot, &keypair);

        trace!("signature {}", shred.signature());
        let nonce = 9;
        let policy = AdmissionPolicy {
            shred_version: shred.version(),
            root: slot.saturating_sub(1),
            max_slot: slot + 1,
            max_data_shreds_per_slot: u32::MAX,
            max_code_shreds_per_slot: u32::MAX,
        };
        let packet = repair_response_packet_from_bytes(
            shred.into_bytes(),
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            nonce,
        )
        .unwrap();
        let bytes = Bytes::copy_from_slice(packet.data(..).unwrap());

        let (parsed, parsed_nonce) = parse_repair(bytes.clone()).unwrap();
        assert_eq!(parsed_nonce, nonce);
        let admissible = parsed.check_policy(&policy).unwrap();
        assert!(admissible.verify(&keypair.pubkey()).is_ok());

        let (parsed, _) = parse_repair(bytes).unwrap();
        let admissible = parsed.check_policy(&policy).unwrap();
        let wrong_keypair = Keypair::new();
        assert!(admissible.verify(&wrong_keypair.pubkey()).is_err());
    }

    #[test]
    fn test_sigverify_shred_repair() {
        run_test_sigverify_shred_repair(0xdead_c0de);
    }
}
