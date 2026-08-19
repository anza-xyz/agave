use {
    crate::{
        cluster_info::ClusterInfo, cluster_info_metrics::GossipStats,
        gossip_command::GossipCommand, gossip_error::GossipError,
        gossip_ingress::ValidatedGossipMessage,
    },
    crossbeam_channel::Sender,
    rayon::ThreadPool,
    solana_perf::packet::{PacketBatch, PacketBatchRecycler},
    solana_pubkey::Pubkey,
    solana_streamer::streamer::ChannelSend,
    std::{
        collections::{HashMap, HashSet},
        sync::{Arc, MutexGuard},
    },
};

/// The subset of [`ClusterInfo`] the gossip engine is allowed to reach.
///
/// The engine holds this instead of an `Arc<ClusterInfo>` so it cannot call a
/// method that submits a command. Those block on a bounded channel the engine
/// itself drains, so a self-submitted command would deadlock the engine against
/// its own queue. The wrapped handle stays private to this module, which is
/// what turns that hazard into a compile error rather than a review item.
#[derive(Clone)]
pub(crate) struct EngineView {
    cluster_info: Arc<ClusterInfo>,
}

impl EngineView {
    pub(crate) fn new(cluster_info: Arc<ClusterInfo>) -> Self {
        Self { cluster_info }
    }

    pub(crate) fn id(&self) -> Pubkey {
        self.cluster_info.id()
    }

    pub(crate) fn stats(&self) -> &GossipStats {
        &self.cluster_info.stats
    }

    pub(crate) fn is_full_alpenglow_epoch(&self) -> bool {
        self.cluster_info.is_full_alpenglow_epoch()
    }

    pub(crate) fn acquire_writer_lease(&self) -> MutexGuard<'_, ()> {
        self.cluster_info.acquire_writer_lease()
    }

    pub(crate) fn detach_command_endpoint(&self, sender: &Arc<Sender<GossipCommand>>) {
        self.cluster_info.detach_command_endpoint(sender)
    }

    pub(crate) fn apply_command(&self, command: GossipCommand) {
        self.cluster_info.apply_command(command)
    }

    pub(crate) fn process_packets(
        &self,
        packets: &mut Vec<Vec<ValidatedGossipMessage>>,
        thread_pool: &ThreadPool,
        recycler: &PacketBatchRecycler,
        response_sender: &impl ChannelSend<PacketBatch>,
        stakes: &HashMap<Pubkey, u64>,
        should_check_duplicate_instance: bool,
    ) -> Result<(), GossipError> {
        self.cluster_info.process_packets(
            packets,
            thread_pool,
            recycler,
            response_sender,
            stakes,
            should_check_duplicate_instance,
        )
    }

    pub(crate) fn run_gossip(
        &self,
        thread_pool: &ThreadPool,
        gossip_validators: Option<&HashSet<Pubkey>>,
        recycler: &PacketBatchRecycler,
        stakes: &HashMap<Pubkey, u64>,
        sender: &impl ChannelSend<PacketBatch>,
        generate_pull_requests: bool,
    ) -> Result<(), GossipError> {
        self.cluster_info.run_gossip(
            thread_pool,
            gossip_validators,
            recycler,
            stakes,
            sender,
            generate_pull_requests,
        )
    }

    pub(crate) fn handle_purge(&self, thread_pool: &ThreadPool, stakes: &HashMap<Pubkey, u64>) {
        self.cluster_info.handle_purge(thread_pool, stakes)
    }

    pub(crate) fn process_entrypoints(&self) -> bool {
        self.cluster_info.process_entrypoints()
    }

    pub(crate) fn refresh_my_gossip_contact_info(&self) {
        self.cluster_info.refresh_my_gossip_contact_info()
    }

    pub(crate) fn refresh_push_active_set(
        &self,
        recycler: &PacketBatchRecycler,
        stakes: &HashMap<Pubkey, u64>,
        gossip_validators: Option<&HashSet<Pubkey>>,
        sender: &impl ChannelSend<PacketBatch>,
    ) {
        self.cluster_info
            .refresh_push_active_set(recycler, stakes, gossip_validators, sender)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::contact_info::ContactInfo, solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace, solana_signer::Signer,
    };

    #[test]
    fn view_forwards_to_cluster_info() {
        let keypair = Arc::new(Keypair::new());
        let pubkey = keypair.pubkey();
        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&pubkey, 0),
            keypair,
            SocketAddrSpace::Unspecified,
        ));
        let view = EngineView::new(Arc::clone(&cluster_info));

        assert_eq!(view.id(), pubkey);
        view.apply_command(GossipCommand::LowestSlot(7));
        assert_eq!(
            cluster_info.lowest_slot_for_tests(pubkey).unwrap().lowest,
            7
        );
    }
}
