//! Pre-sampled fault schedule for deterministic byzfuzz runs.
//!
//! The whole schedule is sampled once from the seed, single-threaded, before
//! the cluster starts.  At interception time the policy is a pure lookup keyed
//! on the message's logical slot — no RNG is consumed in arrival order, so the
//! faults a run applies are a function of the seed alone and a failing run can
//! print its own reproduction recipe.
//!
//! Two fault classes, following ByzzFuzz:
//! - *network faults* isolate a node for a window of slots (any message with an
//!   isolated endpoint is dropped).  These model the environment and apply to
//!   every node, honest or not.
//! - *corruptions* mutate the byzantine node's messages within a window.  The
//!   concrete mutation and its parameters are derived from message content, so
//!   the same message always meets the same fate (and the same vote sent to
//!   different peers can be mutated differently — natural equivocation).
use {
    crate::byzzfuzz::interceptor::AlpenglowRng,
    agave_votor_messages::consensus_message::ConsensusMessage,
    rand::{Rng, SeedableRng},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        hash::{Hash, Hasher},
        ops::RangeInclusive,
    },
};

/// Width of a fault window, in slots.  Matches one leader's window so a fault
/// lands on a coherent unit of protocol activity.
const SLOTS_PER_LEADER_WINDOW: Slot = 4;
/// Number of network faults (isolated-node windows) per schedule.
const DEFAULT_NETWORK_FAULTS: usize = 8;
/// Number of corruption windows per schedule.
const DEFAULT_CORRUPTION_WINDOWS: usize = 8;

/// Isolates a set of nodes for a window of slots.  Any message whose source or
/// destination is isolated is dropped while the window is active.
#[derive(Clone, Debug)]
struct NetworkFault {
    window: RangeInclusive<Slot>,
    isolated: HashSet<Pubkey>,
}

#[derive(Clone, Debug)]
pub(crate) struct FaultSchedule {
    network_faults: Vec<NetworkFault>,
    corruption_windows: Vec<RangeInclusive<Slot>>,
    corruption_seed: u64,
}

impl FaultSchedule {
    /// Sample a schedule deterministically from `seed`.  All randomness is
    /// consumed here, single-threaded, before any cluster thread races.
    pub(crate) fn sample(seed: u64, nodes: &[Pubkey], max_slot: Slot) -> Self {
        let mut rng = AlpenglowRng::seed_from_u64(seed);
        // Isolate one node per network fault so the remaining nodes keep a
        // fully connected core — with four nodes that core always holds a
        // supermajority, so partitions cause skips and recovery, not deadlock.
        let network_faults = (0..DEFAULT_NETWORK_FAULTS)
            .map(|_| {
                let window = random_window(&mut rng, max_slot);
                let isolated = HashSet::from([nodes[rng.random_range(0..nodes.len())]]);
                NetworkFault { window, isolated }
            })
            .collect();
        let corruption_windows = (0..DEFAULT_CORRUPTION_WINDOWS)
            .map(|_| random_window(&mut rng, max_slot))
            .collect();
        let corruption_seed = rng.random();
        Self {
            network_faults,
            corruption_windows,
            corruption_seed,
        }
    }

    /// Whether the link from `source` to `destination` is cut at logical time
    /// `now` (the highest slot consensus has reached).
    pub(crate) fn drops_link(&self, now: Slot, source: &Pubkey, destination: &Pubkey) -> bool {
        self.network_faults.iter().any(|fault| {
            fault.window.contains(&now)
                && (fault.isolated.contains(source) || fault.isolated.contains(destination))
        })
    }

    /// Whether `slot` falls in a window where byzantine messages are corrupted.
    pub(crate) fn in_corruption_window(&self, slot: Slot) -> bool {
        self.corruption_windows
            .iter()
            .any(|window| window.contains(&slot))
    }

    /// Content-derived RNG for choosing and parameterizing a corruption.  Keyed
    /// on message bytes and destination so the choice is reproducible yet still
    /// differs per peer (equivocation), independent of arrival order.
    pub(crate) fn corruption_rng(
        &self,
        message: &ConsensusMessage,
        destination: &Pubkey,
    ) -> AlpenglowRng {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.corruption_seed.hash(&mut hasher);
        destination.hash(&mut hasher);
        if let Ok(bytes) = wincode::serialize(message) {
            bytes.hash(&mut hasher);
        }
        AlpenglowRng::seed_from_u64(hasher.finish())
    }
}

/// The logical slot a message belongs to.
pub(crate) fn message_slot(message: &ConsensusMessage) -> Slot {
    match message {
        ConsensusMessage::Vote(vote) => vote.vote.slot(),
        ConsensusMessage::Certificate(certificate) => certificate.cert_type.slot(),
    }
}

fn random_window(rng: &mut AlpenglowRng, max_slot: Slot) -> RangeInclusive<Slot> {
    let last_start = max_slot.saturating_sub(SLOTS_PER_LEADER_WINDOW).max(1);
    let start = rng.random_range(1..=last_start);
    let end = start
        .saturating_add(SLOTS_PER_LEADER_WINDOW.saturating_sub(1))
        .min(max_slot);
    start..=end
}
