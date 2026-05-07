#![cfg(feature = "agave-unstable-api")]
use {
    crossbeam_channel::Receiver,
    solana_perf::packet::{
        BytesPacket, Meta, PACKET_DATA_SIZE, PacketBatch, PacketRef, bytes::Bytes,
    },
    std::sync::Arc,
};

pub type BankingPacketBatch = Arc<PacketBatch>;
pub type BankingPacketReceiver = Receiver<BankingPacketBatch>;

pub fn bytes_packet_from_bytes(bytes: Vec<u8>) -> BytesPacket {
    let buffer = Bytes::from(bytes);
    let mut meta = Meta::default();
    meta.size = buffer.len();
    BytesPacket::new(buffer, meta)
}

pub fn bytes_packet_from_transaction<T>(transaction: &T) -> BytesPacket
where
    T: wincode::SchemaWrite<wincode::config::DefaultConfig, Src = T> + ?Sized,
{
    let wire_transaction = wincode::serialize(transaction).expect("serialize transaction");
    assert!(
        wire_transaction.len() <= PACKET_DATA_SIZE,
        "serialized transaction does not fit in packet"
    );
    bytes_packet_from_bytes(wire_transaction)
}

pub fn packet_batch_from_transaction<T>(transaction: &T) -> PacketBatch
where
    T: wincode::SchemaWrite<wincode::config::DefaultConfig, Src = T> + ?Sized,
{
    PacketBatch::Single(bytes_packet_from_transaction(transaction))
}

pub fn banking_packet_batch_from_packet_ref(packet: PacketRef<'_>) -> BankingPacketBatch {
    BankingPacketBatch::new(PacketBatch::Single(packet.to_bytes_packet()))
}

pub fn banking_packet_batch_from_bytes(bytes: Vec<u8>) -> BankingPacketBatch {
    BankingPacketBatch::new(PacketBatch::Single(bytes_packet_from_bytes(bytes)))
}

pub fn banking_packet_batch_from_transaction<T>(transaction: &T) -> BankingPacketBatch
where
    T: wincode::SchemaWrite<wincode::config::DefaultConfig, Src = T> + ?Sized,
{
    BankingPacketBatch::new(packet_batch_from_transaction(transaction))
}
