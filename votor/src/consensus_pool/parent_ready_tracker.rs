//! Tracks the parent-ready condition
//!
//! The parent-ready condition pertains to a slot `s` and a block hash `hash(b)`,
//! where `s` is the first slot of a leader window and `s > slot(b)`.
//! Specifically, it is defined as the following:
//!   - Block `b` is notarized or notarized-fallback, and
//!   - slots `slot(b) + 1` (inclusive) to `s` (non-inclusive) are skip-certified.
//!
//! Additional restriction on notarization votes ensure that the parent-ready
//! condition holds for a block `b` only if it also holds for all ancestors of `b`.
//! Together this ensures that the block `b` is a valid parent for block
//! production, i.e., under good network conditions an honest leader proposing
//! a block with parent `b` in slot `s` will have their block finalized.

use {
    crate::{common::MAX_NOTAR_FALLBACK_BLOCKS, event::VotorEvent},
    agave_votor_messages::consensus_message::Block,
    core::fmt,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    std::{collections::HashMap, sync::Arc},
};

pub(crate) type ParentReady = (Slot, Block);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockProductionParent {
    MissedWindow,
    ParentNotReady,
    Parent(Block),
}

struct DebugIgnore<T>(T);

impl<T> fmt::Debug for DebugIgnore<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ignored>")
    }
}

#[derive(Debug)]
pub(crate) struct ParentReadyTracker {
    cluster_info: DebugIgnore<Arc<ClusterInfo>>,
    /// Parent ready status for each slot
    slot_statuses: HashMap<Slot, ParentReadyStatus>,
    /// Root
    root: Slot,
    /// Highest slot with parent ready status
    // TODO: While the voting loop is sequential we track every slot (not just the first in window)
    // However once we handle all slots concurrently we will update this to only count first leader
    // slot in window
    highest_with_parent_ready: Slot,
}

#[derive(Clone, Default, Debug)]
struct ParentReadyStatus {
    /// Whether this slot has a skip certificate
    skip: bool,
    /// The blocks that have been notar fallbacked in this slot
    notar_fallbacks: Vec<Block>,
    /// The parent blocks that achieve parent ready in this slot,
    /// Theses blocks are all potential parents choosable in this slot
    parents_ready: Vec<Block>,
}

impl ParentReadyTracker {
    /// Creates a new tracker with the selected startup parent-ready state.
    pub(super) fn new(
        cluster_info: Arc<ClusterInfo>,
        root: Slot,
        (slot, parent_block): ParentReady,
    ) -> Self {
        debug_assert!(parent_block.slot < slot);
        let mut slot_statuses = HashMap::new();
        // Parent block is notarize fallback
        slot_statuses.insert(
            parent_block.slot,
            ParentReadyStatus {
                skip: false,
                notar_fallbacks: vec![parent_block],
                parents_ready: vec![],
            },
        );
        // Intermediate blocks have skips
        for s in (parent_block.slot.checked_add(1).unwrap())..slot {
            slot_statuses.insert(
                s,
                ParentReadyStatus {
                    skip: true,
                    notar_fallbacks: vec![],
                    parents_ready: vec![parent_block],
                },
            );
        }
        // Initial slot is parent ready
        slot_statuses.insert(
            slot,
            ParentReadyStatus {
                skip: false,
                notar_fallbacks: vec![],
                parents_ready: vec![parent_block],
            },
        );

        Self {
            cluster_info: DebugIgnore(cluster_info),
            slot_statuses,
            root: parent_block.slot.min(root),
            highest_with_parent_ready: slot.max(root),
        }
    }

    /// Adds a new notarize fallback certificate, we can use Notarize/NotarizeFallback/FastFinalize
    pub(super) fn add_new_notar_fallback_or_stronger(
        &mut self,
        block: Block,
        events: &mut Vec<VotorEvent>,
    ) {
        if block.slot <= self.root {
            return;
        }

        let status = self.slot_statuses.entry(block.slot).or_default();
        if status.notar_fallbacks.contains(&block) {
            return;
        }
        trace!(
            "{}: Adding new notar fallback for {block:?}",
            self.cluster_info.0.id()
        );
        status.notar_fallbacks.push(block);
        assert!(status.notar_fallbacks.len() <= MAX_NOTAR_FALLBACK_BLOCKS);

        // Add this block as valid parent to skip connected future blocks
        for s in block.slot.saturating_add(1).. {
            trace!(
                "{}: Adding new parent ready for {s} parent {block:?}",
                self.cluster_info.0.id()
            );
            let status = self.slot_statuses.entry(s).or_default();
            if !status.parents_ready.contains(&block) {
                status.parents_ready.push(block);

                // Only notify for parent ready on first leader slots
                if s.is_multiple_of(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot) {
                    events.push(VotorEvent::ParentReady {
                        slot: s,
                        parent_block: block,
                    });
                }

                self.highest_with_parent_ready = s.max(self.highest_with_parent_ready);
            }

            if !status.skip {
                break;
            }
        }
    }

    /// Adds a new skip certificate
    pub(super) fn add_new_skip(&mut self, slot: Slot, events: &mut Vec<VotorEvent>) {
        if slot <= self.root {
            return;
        }

        trace!("{}: Adding new skip for {slot:?}", self.cluster_info.0.id());
        let status = self.slot_statuses.entry(slot).or_default();
        status.skip = true;

        // Get newly connected future slots
        let mut future_slots = vec![];
        for s in slot.saturating_add(1).. {
            future_slots.push(s);
            if !self.slot_statuses.get(&s).is_some_and(|ss| ss.skip) {
                break;
            }
        }

        // Find possible parents using the previous slot
        let mut potential_parents = vec![];
        let Some(status) = self.slot_statuses.get(&(slot.saturating_sub(1))) else {
            return;
        };
        for nf in &status.notar_fallbacks {
            // If there's a notarize fallback certificate we can use the previous slot
            // as a parent
            potential_parents.push(*nf);
        }
        if status.skip {
            // If there's a skip certificate we can use the parents of the previous slot
            // as a parent
            for parent in &status.parents_ready {
                potential_parents.push(*parent);
            }
        }

        if potential_parents.is_empty() {
            return;
        }

        // Add these as valid parents to the future slots
        for s in future_slots {
            trace!(
                "{}: Adding new parent ready for {s} parents {potential_parents:?}",
                self.cluster_info.0.id(),
            );
            let status = self.slot_statuses.entry(s).or_default();
            for &block in &potential_parents {
                if status.parents_ready.contains(&block) {
                    // We already have this parent ready
                    continue;
                }
                status.parents_ready.push(block);
                // Only notify for parent ready on first leader slots
                if s.is_multiple_of(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot) {
                    events.push(VotorEvent::ParentReady {
                        slot: s,
                        parent_block: block,
                    });
                }
            }

            self.highest_with_parent_ready = s.max(self.highest_with_parent_ready);
        }
    }

    #[cfg(test)]
    fn parent_ready(&self, slot: Slot, parent: Block) -> bool {
        self.slot_statuses
            .get(&slot)
            .is_some_and(|ss| ss.parents_ready.contains(&parent))
    }

    /// For our leader slot `slot`, which block should we use as the parent
    pub(crate) fn block_production_parent(&self, slot: Slot) -> BlockProductionParent {
        if self.highest_parent_ready() > slot {
            // This indicates that our block has already received a certificate
            // either because we were too slow, or because we are restarting
            // and catching up. Either way we should not attempt to produce this slot
            return BlockProductionParent::MissedWindow;
        }
        match self
            .slot_statuses
            .get(&slot)
            .and_then(|ss| ss.parents_ready.iter().min().copied())
        {
            Some(parent) => BlockProductionParent::Parent(parent),
            // TODO: this will be plugged in for optimistic block production
            None => BlockProductionParent::ParentNotReady,
        }
    }

    /// Returns whether the block is notarized or notarized-fallback
    ///
    /// This includes any special cases like: genesis, snapshot root, migration Genesis cert
    pub(super) fn has_notar_fallback_or_stronger(&self, block: Block) -> bool {
        self.slot_statuses
            .get(&block.slot)
            .is_some_and(|ss| ss.notar_fallbacks.contains(&block))
    }

    fn highest_parent_ready(&self) -> Slot {
        self.highest_with_parent_ready
    }

    pub(super) fn set_root(&mut self, root: Slot) {
        self.root = root;
        self.slot_statuses.retain(|&s, _| s >= root);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::tests::get_cluster_info, itertools::Itertools, solana_hash::Hash,
        solana_keypair::Keypair,
    };

    fn root_parent_ready(root_block: Block) -> ParentReady {
        (root_block.slot.saturating_add(1), root_block)
    }

    fn new_tracker(cluster_info: Arc<ClusterInfo>, root_block: Block) -> ParentReadyTracker {
        ParentReadyTracker::new(cluster_info, root_block.slot, root_parent_ready(root_block))
    }

    #[test]
    fn basic() {
        let cluster_info = get_cluster_info(Keypair::new());
        let genesis = Block::default();
        let mut tracker = new_tracker(cluster_info, genesis);
        let mut events = vec![];

        for i in 1..2 * NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot {
            let block = Block {
                slot: i,
                block_id: Hash::new_unique(),
            };
            tracker.add_new_notar_fallback_or_stronger(block, &mut events);
            assert_eq!(tracker.highest_parent_ready(), i + 1);
            assert!(tracker.parent_ready(i + 1, block));
        }
    }

    #[test]
    fn skips() {
        let cluster_info = get_cluster_info(Keypair::new());
        let genesis = Block::default();
        let mut tracker = new_tracker(cluster_info, genesis);
        let mut events = vec![];
        let block = Block {
            slot: 1,
            block_id: Hash::new_unique(),
        };

        tracker.add_new_notar_fallback_or_stronger(block, &mut events);
        tracker.add_new_skip(1, &mut events);
        tracker.add_new_skip(2, &mut events);
        tracker.add_new_skip(3, &mut events);

        assert!(tracker.parent_ready(4, block));
        assert!(tracker.parent_ready(4, genesis));
        assert_eq!(tracker.highest_parent_ready(), 4);
    }

    #[test]
    fn out_of_order() {
        let cluster_info = get_cluster_info(Keypair::new());
        let genesis = Block::default();
        let mut tracker = new_tracker(cluster_info, genesis);
        let mut events = vec![];
        let block = Block {
            slot: 1,
            block_id: Hash::new_unique(),
        };

        tracker.add_new_skip(3, &mut events);
        tracker.add_new_skip(2, &mut events);

        tracker.add_new_notar_fallback_or_stronger(block, &mut events);
        assert!(tracker.parent_ready(4, block));
        assert!(!tracker.parent_ready(4, genesis));

        tracker.add_new_skip(1, &mut events);
        assert!(tracker.parent_ready(4, block));
        assert!(tracker.parent_ready(4, genesis));
    }

    #[test]
    fn snapshot_wfsm() {
        let cluster_info = get_cluster_info(Keypair::new());
        let root_slot = 2147;
        let root_block = Block {
            slot: root_slot,
            block_id: Hash::new_unique(),
        };
        let mut tracker = new_tracker(cluster_info, root_block);
        let mut events = vec![];

        assert!(tracker.parent_ready(root_slot + 1, root_block));
        assert_eq!(tracker.highest_parent_ready(), root_slot + 1);

        // Skipping root slot shouldn't do anything
        tracker.add_new_skip(root_slot, &mut events);
        assert!(tracker.parent_ready(root_slot + 1, root_block));
        assert_eq!(tracker.highest_parent_ready(), root_slot + 1);

        // Adding new certs should work as root slot is implicitely notarized fallback
        tracker.add_new_skip(root_slot + 1, &mut events);
        tracker.add_new_skip(root_slot + 2, &mut events);
        assert!(tracker.parent_ready(root_slot + 3, root_block));
        assert_eq!(tracker.highest_parent_ready(), root_slot + 3);

        let block = Block {
            slot: root_slot + 4,
            block_id: Hash::new_unique(),
        };
        tracker.add_new_notar_fallback_or_stronger(block, &mut events);
        assert!(tracker.parent_ready(root_slot + 3, root_block));
        assert!(tracker.parent_ready(root_slot + 5, block));
        assert_eq!(tracker.highest_parent_ready(), root_slot + 5);
    }

    #[test]
    fn restored_parent_ready() {
        let cluster_info = get_cluster_info(Keypair::new());
        let root_slot = 4;
        let restored = Block {
            slot: 13,
            block_id: Hash::new_unique(),
        };
        let tracker = ParentReadyTracker::new(cluster_info, root_slot, (16, restored));

        assert!(tracker.parent_ready(16, restored));
        assert_eq!(tracker.highest_parent_ready(), 16);
        assert_eq!(
            tracker.block_production_parent(16),
            BlockProductionParent::Parent(restored)
        );
    }

    #[test]
    fn highest_parent_ready_out_of_order() {
        let cluster_info = get_cluster_info(Keypair::new());
        let genesis = Block::default();
        let mut tracker = new_tracker(cluster_info, genesis);
        let mut events = vec![];
        assert_eq!(tracker.highest_parent_ready(), 1);

        tracker.add_new_skip(2, &mut events);
        assert_eq!(tracker.highest_parent_ready(), 1);

        tracker.add_new_skip(3, &mut events);
        assert_eq!(tracker.highest_parent_ready(), 1);

        tracker.add_new_skip(1, &mut events);
        assert!(tracker.parent_ready(4, genesis));
        assert_eq!(tracker.highest_parent_ready(), 4);
        assert_eq!(
            tracker.block_production_parent(4),
            BlockProductionParent::Parent(genesis)
        );
    }

    #[test]
    fn missed_window() {
        let cluster_info = get_cluster_info(Keypair::new());
        let genesis = Block::default();
        let mut tracker = new_tracker(cluster_info, genesis);
        let mut events = vec![];
        assert_eq!(tracker.highest_parent_ready(), 1);
        assert_eq!(
            tracker.block_production_parent(4),
            BlockProductionParent::ParentNotReady
        );

        tracker.add_new_notar_fallback_or_stronger(
            Block {
                slot: 4,
                block_id: Hash::new_unique(),
            },
            &mut events,
        );
        assert_eq!(tracker.highest_parent_ready(), 5);
        assert_eq!(
            tracker.block_production_parent(4),
            BlockProductionParent::MissedWindow
        );

        assert_eq!(
            tracker.block_production_parent(8),
            BlockProductionParent::ParentNotReady
        );
        tracker.add_new_notar_fallback_or_stronger(
            Block {
                slot: 64,
                block_id: Hash::new_unique(),
            },
            &mut events,
        );
        assert_eq!(tracker.highest_parent_ready(), 65);
        assert_eq!(
            tracker.block_production_parent(8),
            BlockProductionParent::MissedWindow
        );
    }

    #[test]
    fn pick_more_skips() {
        let cluster_info = get_cluster_info(Keypair::new());
        let genesis = Block::default();
        let mut tracker = new_tracker(cluster_info, genesis);
        let mut events = vec![];

        for i in 1..=10 {
            tracker.add_new_skip(i, &mut vec![]);
            tracker.add_new_notar_fallback_or_stronger(
                Block {
                    slot: i,
                    block_id: Hash::new_unique(),
                },
                &mut vec![],
            );
        }

        tracker.add_new_skip(11, &mut events);

        assert_eq!(12, tracker.highest_parent_ready(),);
        let parent_readys: Vec<Slot> = events
            .into_iter()
            .map(|event| match event {
                VotorEvent::ParentReady { slot, parent_block } => {
                    assert!(slot == 12);
                    parent_block.slot
                }
                _ => panic!("Invalid event"),
            })
            .sorted()
            .collect();
        assert_eq!(parent_readys, (0..=10).collect::<Vec<Slot>>());
        assert_eq!(
            tracker.block_production_parent(12),
            BlockProductionParent::Parent(genesis)
        );
    }
}
