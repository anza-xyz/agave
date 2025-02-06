use {
    solana_connection_cache::connection_cache::Protocol, solana_sdk::clock::Slot,
    std::net::SocketAddr,
};

/// A trait to abstract out the leader estimation for the
/// SendTransactionService.
pub trait TpuInfo {
    fn refresh_recent_peers(&mut self);

    /// Takes `max_count` which specifies how many leaders per
    /// `NUM_CONSECUTIVE_LEADER_SLOTS` we want to receive and returns *unique*
    /// TPU socket addresses for these leaders.
    ///
    /// For example, if leader schedule was `[L1, L1, L1, L1, L2, L2, L2, L2,
    /// L1, ...]` it will return `[L1, L2]` (the last L1 will be not added to
    /// the result).
    fn get_leader_tpus(&self, max_count: u64, protocol: Protocol) -> Vec<&SocketAddr>;

    /// Takes `max_count` which specifies how many leaders per
    /// `NUM_CONSECUTIVE_LEADER_SLOTS` we want to receive and returns TPU socket
    /// addresses for these leaders.
    ///
    /// For example, if leader schedule was `[L1, L1, L1, L1, L2, L2, L2, L2,
    /// L1, ...]` it will return `[L1, L2, L1]`.
    #[allow(
        dead_code,
        reason = "This function will be used when tpu-client-next will be added to this module."
    )]
    fn get_not_unique_leader_tpus(&self, max_count: u64, protocol: Protocol) -> Vec<&SocketAddr>;

    /// In addition to the tpu address, also return the leader slot
    #[deprecated(since = "2.2.0", note = "This function is not used anywhere.")]
    fn get_leader_tpus_with_slots(
        &self,
        max_count: u64,
        protocol: Protocol,
    ) -> Vec<(&SocketAddr, Slot)>;
}

#[derive(Clone)]
pub struct NullTpuInfo;

impl TpuInfo for NullTpuInfo {
    fn refresh_recent_peers(&mut self) {}
    fn get_leader_tpus(&self, _max_count: u64, _protocol: Protocol) -> Vec<&SocketAddr> {
        vec![]
    }
    fn get_not_unique_leader_tpus(&self, _max_count: u64, _protocol: Protocol) -> Vec<&SocketAddr> {
        vec![]
    }

    fn get_leader_tpus_with_slots(
        &self,
        _max_count: u64,
        _protocol: Protocol,
    ) -> Vec<(&SocketAddr, Slot)> {
        vec![]
    }
}
