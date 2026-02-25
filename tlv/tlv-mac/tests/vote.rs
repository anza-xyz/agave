#![allow(clippy::arithmetic_side_effects)]
use {
    bytes::{Bytes, BytesMut},
    solana_tlv::*,
    solana_tlv_mac::Signature,
    std::net::SocketAddr,
    wincode::{SchemaRead, SchemaWrite, containers, len::ShortU16Len},
};

#[derive(Debug, Clone, SchemaRead, SchemaWrite, PartialEq, Eq)]
struct Finalize {
    pubkey: [u8; 32],
    bls_signature: [u8; 96],
}

#[derive(Debug, Clone, SchemaRead, SchemaWrite, PartialEq, Eq)]
struct NotarizeCert {
    bls_signature: [u8; 96],
    #[wincode(with = "containers::Vec<u8, ShortU16Len>")]
    bitmap: Vec<u8>,
}

define_tlv_enum! (pub(crate) enum AlepnglowVotor {
    1=>Nonce(u64),
    2=>Mac(Signature<16>),
    10=>Finalize(Finalize),
    11=>NotarizeCert(NotarizeCert),
});

fn main() {
    let notar_cert = NotarizeCert {
        bitmap: vec![42u8; 2000 / 8],
        bls_signature: [7; 96],
    };
    let final_vote = Finalize {
        pubkey: [3; 32],
        bls_signature: [7; 96],
    };
    let ag_nonce = 1231244;

    // allocate space for a packet and fill it with data
    let mut buffer = BytesMut::with_capacity(1200);
    let entries = [
        AlepnglowVotor::Nonce(ag_nonce),
        AlepnglowVotor::Finalize(final_vote),
        AlepnglowVotor::NotarizeCert(notar_cert),
    ];
    serialize_into_buffer(&entries, &mut buffer).unwrap();

    // sign packet
    let src: SocketAddr = "1.2.3.4:8888".parse().unwrap();
    let dst: SocketAddr = "5.6.7.8:8888".parse().unwrap();
    let key = [1; 32];
    let mut nonce = [0u8; 12];
    nonce[0..8].copy_from_slice(&ag_nonce.to_be_bytes());
    let signature: Signature<16> =
        Signature::new_poly1305_for_udp(src, dst, &key, &nonce, &buffer[..buffer.len()]);
    // write signature into the packet
    serialize_into_buffer(&[AlepnglowVotor::Mac(signature)], &mut buffer).unwrap();
    let buffer = buffer.freeze();

    let entries_rx = decode_and_verify_signature(src, dst, key, buffer.clone()).unwrap();
    assert_eq!(
        entries_rx[0..=2],
        entries,
        "The original entries should match up"
    );

    // try replaying a valid message to a wrong address
    let dst2: SocketAddr = "1.2.5.4:8888".parse().unwrap();

    assert_eq!(
        decode_and_verify_signature(src, dst2, key, buffer).unwrap_err(),
        "Invalid packet!"
    );
}

fn decode_and_verify_signature(
    src: SocketAddr,
    dst: SocketAddr,
    key: [u8; 32],
    buffer: Bytes,
) -> Result<Vec<AlepnglowVotor>, String> {
    // decode and verify the signature
    let mut recovered = Vec::new();
    let mut signed_portion = 0;
    let mut nonce = [0u8; 12];
    for maybe_record in TlvIter::new(buffer.clone()) {
        let size = maybe_record.serialized_size();
        let maybe_record: Result<AlepnglowVotor, _> = maybe_record.try_into();
        let record = match maybe_record {
            Ok(record) => record,
            Err(e) => Err(e.to_string())?,
        };

        match record {
            AlepnglowVotor::Mac(signature) => {
                let correct_signature: Signature<16> = Signature::new_poly1305_for_udp(
                    src,
                    dst,
                    &key,
                    &nonce,
                    &buffer[..signed_portion],
                );
                if signature != correct_signature {
                    return Err("Invalid packet!".to_owned());
                }
                recovered.push(record);
                break; // do not read past signed portion!
            }
            AlepnglowVotor::Nonce(rx_nonce) => {
                nonce[0..8].copy_from_slice(&rx_nonce.to_be_bytes());
            }
            _ => recovered.push(record),
        }
        signed_portion += size;
    }
    Ok(recovered)
}
