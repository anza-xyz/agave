use {
    async_trait::async_trait,
    solana_clock::{Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_connection_cache::connection_cache::Protocol,
    solana_gossip::cluster_info::ClusterInfo,
    solana_poh::poh_recorder::PohRecorder,
    solana_tpu_client_next::leader_updater::LeaderUpdater,
    std::{
        net::SocketAddr,
        sync::{Arc, RwLock},
    },
};

// Warning: this module assumes a leader window starts at a slot which is a multiple of NUM_CONSECUTIVE_LEADER_SLOTS.
// Leader windows are identified by (any slot in window)/NUM_CONSECUTIVE_LEADER_SLOTS.

/// Number of leader windows in cache.
/// Gossip database is queried when less than half of this buffer contains upcoming leaders.
/// So must be at least lookahead_leaders * 2
/// We accept to not adapt to an address change appearing on Gossip less than
/// BUF_SIZE_WINDOWS * NUM_CONSECUTIVE_LEADER_SLOTS slots before the node becomes leader.
/// 8 with 400ms slots => ~ 13 seconds
const BUF_SIZE_WINDOWS: u64 = 8;

pub struct UpcomingLeadersCache {
    protocol: Protocol,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    cluster_info: Arc<ClusterInfo>,
    /// First window contained in the cache, inclusive
    /// If greater or equal than window_end, the cache is empty.
    window_start: Slot,
    /// Last window contained in the cache, exclusive
    window_end: Slot,
    /// Ring buffer. The leader of window w is stored at w%BUF_SIZE_WINDOWS.
    buf: [Option<SocketAddr>; BUF_SIZE_WINDOWS as usize]
}

impl UpcomingLeadersCache {
    pub fn new(poh_recorder: Arc<RwLock<PohRecorder>>, cluster_info: Arc<ClusterInfo>, protocol: Protocol) -> Self {
        UpcomingLeadersCache {
            protocol,
            poh_recorder,
            cluster_info,
            window_start: 0,
            window_end: 0,
            buf: [None; BUF_SIZE_WINDOWS as usize],
        }
    }

    /// Request leader adresses between from_window (inclusive) and to_window (exclusive).
    /// Fill `res_tpu_vote` vector with what we have in the buffer.
    /// Return the first window not found.
    fn get(&self, mut from_window: Slot, to_window: Slot, res_tpu_vote: &mut Vec<SocketAddr>) -> Slot {
        while (self.window_start..self.window_end).contains(&from_window) && from_window < to_window {
            if let Some(addr) = self.buf[(from_window % BUF_SIZE_WINDOWS) as usize] {
                res_tpu_vote.push(addr);
            }
            from_window += 1;
        }
        from_window
    }

    /// Fetch information from leader schedule and Gossip starting `from_window` to fill out the buffer
    /// (preserving entry self.window_start).
    /// from_window >= window_start
    fn fill(&mut self, from_window: Slot) {
        let poh_recorder = self.poh_recorder.read().unwrap();
        let mut leaders = Vec::with_capacity(BUF_SIZE_WINDOWS as usize);
        let mut window = from_window;
        let idx_to_keep = if self.window_start < self.window_end {
            self.window_start
        } else {
            // Nothing valuable in the buffer
            window
        } % BUF_SIZE_WINDOWS;
        loop {
            let leader = poh_recorder.leader_of_slot(window * NUM_CONSECUTIVE_LEADER_SLOTS);
            leaders.push(leader);
            self.buf[(window%BUF_SIZE_WINDOWS) as usize] = None;
            window += 1;
            if window%BUF_SIZE_WINDOWS == idx_to_keep {
                break;
            }
        }
        drop(poh_recorder);
        self.window_end = window;
        self.cluster_info.lookup_contact_infos(&leaders, |idx, node| {
            self.buf[((from_window + idx as u64) % BUF_SIZE_WINDOWS) as usize] = node.tpu_vote(self.protocol);
        });
    }
}

#[async_trait]
impl LeaderUpdater for UpcomingLeadersCache {
    /// Ensure at least 1 TPU is returned, returning self TPU if no other, and no more than lookahead_leaders
    /// [ Not sure about the interest to return self TPU ]
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        let slot = self.poh_recorder.read().unwrap().current_poh_slot();
        let from_window = slot / NUM_CONSECUTIVE_LEADER_SLOTS;
        let to_window = from_window + lookahead_leaders as u64; // exclusive

        self.window_start = from_window; // We can forget all previous windows.
        // If ever the current poh is decreasing for some reason, we can have a
        // a cache miss, but it's not a big deal.

        let mut res_tpu_vote = Vec::with_capacity(lookahead_leaders);
        let window = self.get(from_window, to_window, &mut res_tpu_vote);
        if window < to_window {
            // window >= window_end
            warn!("Upcoming leaders cache miss");
            self.fill(window);
            if self.get(window, to_window, &mut res_tpu_vote) < to_window {
                warn!("Could not fill requested upcoming leaders"); // Should never happen
            }
        }
        if res_tpu_vote.is_empty() {
            if let Some(me) = self.cluster_info.my_contact_info().tpu(self.protocol) {
                res_tpu_vote.push(me);
            }
        } else {
            res_tpu_vote.sort_unstable();
            res_tpu_vote.dedup();
        }
        res_tpu_vote
    }

    /// Fill the buffer if it's at least half empty
    /// Only call if `next_leaders` has been called at least once,
    /// so that window_start and window_end are initialized.
    fn update(&mut self) {
        if self.window_end < self.window_start + BUF_SIZE_WINDOWS / 2 {
            self.fill(self.window_end);
        }
    }

    async fn stop(&mut self) {
    }
}