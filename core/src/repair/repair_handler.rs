use {
    super::{
        malicious_repair_handler::{MaliciousRepairConfig, MaliciousRepairHandler},
        serve_repair::ServeRepair,
        standard_repair_handler::StandardRepairHandler,
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{blockstore::Blockstore, shred::Nonce},
    solana_perf::packet::{PacketBatch, PacketBatchRecycler},
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    std::{
        collections::HashSet,
        net::SocketAddr,
        sync::{Arc, RwLock},
    },
};

pub trait RepairHandler {
    fn run_window_request(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        shred_index: u64,
        nonce: Nonce,
    ) -> Option<PacketBatch>;

    fn run_highest_window_request(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        highest_index: u64,
        nonce: Nonce,
    ) -> Option<PacketBatch>;

    fn run_orphan(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        max_responses: usize,
        nonce: Nonce,
    ) -> Option<PacketBatch>;

    fn run_ancestor_hashes(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        slot: Slot,
        nonce: Nonce,
    ) -> Option<PacketBatch>;
}

#[derive(Clone, Debug, Default)]
pub enum RepairHandlerType {
    #[default]
    Standard,
    Malicious(MaliciousRepairConfig),
}

impl RepairHandlerType {
    pub fn to_handler(&self, blockstore: Arc<Blockstore>) -> Box<dyn RepairHandler + Send + Sync> {
        match self {
            RepairHandlerType::Standard => Box::new(StandardRepairHandler::new(blockstore)),
            RepairHandlerType::Malicious(config) => {
                Box::new(MaliciousRepairHandler::new(blockstore, *config))
            }
        }
    }

    pub fn create_serve_repair(
        &self,
        blockstore: Arc<Blockstore>,
        cluster_info: Arc<ClusterInfo>,
        bank_forks: Arc<RwLock<BankForks>>,
        serve_repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
    ) -> ServeRepair {
        ServeRepair::new(
            cluster_info,
            bank_forks,
            serve_repair_whitelist,
            self.to_handler(blockstore),
        )
    }
}
