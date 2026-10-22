#![cfg(feature = "agave-unstable-api")]
use {
    crossbeam_channel::Receiver,
    solana_perf::packet::PacketBatch,
    std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {
    solana_perf::packet::{BytesPacket, BytesPacketBatch, Meta, PACKET_DATA_SIZE, bytes::Bytes},
    wincode::{SchemaWrite, config::DefaultConfig},
};

pub type BankingPacketBatch = Arc<PacketBatch>;
pub type BankingPacketReceiver = Receiver<BankingPacketBatch>;

#[cfg(feature = "dev-context-only-utils")]
fn to_bytes_packet<T>(item: &T) -> BytesPacket
where
    T: SchemaWrite<DefaultConfig, Src = T> + ?Sized,
{
    let buffer = Bytes::from(wincode::serialize(item).expect("serialize request"));
    assert!(buffer.len() <= PACKET_DATA_SIZE);
    let mut meta = Meta::default();
    meta.size = buffer.len();
    BytesPacket::new(buffer, meta)
}

#[cfg(feature = "dev-context-only-utils")]
fn to_single_packet_batch<T>(item: &T) -> PacketBatch
where
    T: SchemaWrite<DefaultConfig, Src = T> + ?Sized,
{
    PacketBatch::Single(to_bytes_packet(item))
}

#[cfg(feature = "dev-context-only-utils")]
fn to_packet_batch<T>(items: &[T]) -> PacketBatch
where
    T: SchemaWrite<DefaultConfig, Src = T>,
{
    if let [item] = items {
        return to_single_packet_batch(item);
    }

    items
        .iter()
        .map(to_bytes_packet)
        .collect::<BytesPacketBatch>()
        .into()
}

#[cfg(feature = "dev-context-only-utils")]
pub fn to_banking_packet_batch<T>(items: &[T]) -> BankingPacketBatch
where
    T: SchemaWrite<DefaultConfig, Src = T>,
{
    Arc::new(to_packet_batch(items))
}

#[cfg(feature = "dev-context-only-utils")]
pub fn to_single_banking_packet_batch<T>(item: &T) -> BankingPacketBatch
where
    T: SchemaWrite<DefaultConfig, Src = T> + ?Sized,
{
    Arc::new(to_single_packet_batch(item))
}

/// Priority floor shared from the banking-stage scheduler to sigverify.
///
/// When saturated, the scheduler publishes the queue-min transaction's
/// priority. Sigverify drops at-or-below-floor arrivals.
/// In practice, transactions always have non-zero priorities.
#[derive(Debug)]
pub struct SchedulerPriorityFloor(AtomicU64);

impl SchedulerPriorityFloor {
    pub fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn set(&self, floor: u64) {
        self.0.store(floor, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.set(0);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for SchedulerPriorityFloor {
    fn default() -> Self {
        Self::new()
    }
}
