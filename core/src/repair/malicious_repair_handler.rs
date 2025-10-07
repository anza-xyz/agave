use {
    super::{repair_handler::RepairHandler, repair_response::repair_response_packet_from_bytes},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{Nonce, SIZE_OF_DATA_SHRED_HEADERS},
    },
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler},
    std::{net::SocketAddr, sync::Arc},
};

#[derive(Copy, Clone, Debug, Default)]
pub struct MaliciousRepairConfig {
    bad_shred_slot_frequency: Option<Slot>,
}

pub struct MaliciousRepairHandler {
    blockstore: Arc<Blockstore>,
    config: MaliciousRepairConfig,
}

impl MaliciousRepairHandler {
    const BAD_DATA_INDEX: usize = SIZE_OF_DATA_SHRED_HEADERS + 5;

    pub fn new(blockstore: Arc<Blockstore>, config: MaliciousRepairConfig) -> Self {
        Self { blockstore, config }
    }
}

impl RepairHandler for MaliciousRepairHandler {
    fn blockstore(&self) -> &Blockstore {
        &self.blockstore
    }

    fn repair_response_packet(
        &self,
        slot: Slot,
        shred_index: u64,
        block_id: Option<Hash>,
        dest: &SocketAddr,
        nonce: Nonce,
    ) -> Option<Packet> {
        let mut shred = match block_id {
            None => self.blockstore.get_data_shred(slot, shred_index),
            Some(block_id) => {
                let location = self
                    .blockstore()
                    .get_block_location(slot, block_id)
                    .expect("Unable to fetch block location from blockstore")?;
                self.blockstore()
                    .get_data_shred_from_location(slot, shred_index, location)
            }
        }
        .expect("Blockstore could not get data shred")?;

        if self
            .config
            .bad_shred_slot_frequency
            .is_some_and(|freq| slot % freq == 0)
        {
            // Change some random piece of data
            shred[Self::BAD_DATA_INDEX] = shred[Self::BAD_DATA_INDEX].wrapping_add(1);
        }
        repair_response_packet_from_bytes(shred, dest, nonce)
    }

    fn run_orphan(
        &self,
        _recycler: &PacketBatchRecycler,
        _from_addr: &SocketAddr,
        _slot: Slot,
        _block_id: Option<Hash>,
        _max_responses: usize,
        _nonce: Nonce,
    ) -> Option<PacketBatch> {
        // Malicious orphan response will come later
        None
    }
}
