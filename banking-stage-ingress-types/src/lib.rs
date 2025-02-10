use {crossbeam_channel::Receiver, solana_perf::packet::Packet, std::sync::Arc};

pub type BankingPacketBatch = Arc<Vec<Vec<Packet>>>;
pub type BankingPacketReceiver = Receiver<BankingPacketBatch>;
