#![cfg(feature = "agave-unstable-api")]
use {crossbeam_channel::Receiver, solana_perf::packet::PacketBatch, std::sync::Arc};

pub type BankingPacketBatch = Arc<Vec<PacketBatch>>;
pub type BankingPacketReceiver = Receiver<BankingPacketBatch>;
