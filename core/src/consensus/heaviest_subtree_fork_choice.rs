#[cfg(test)]
use trees::{Tree, TreeWalk};
use {
    crate::consensus::tree_diff::TreeDiff,
    ahash::AHashMap,
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::VersionedEpochStakes,
    std::{
        borrow::Borrow,
        cmp::Ordering,
        collections::{BTreeMap, BTreeSet, HashMap, VecDeque, btree_set::Iter, hash_map::Entry},
        time::{Duration, Instant},
    },
};

pub type ForkWeight = u64;
pub type SlotHashKey = (Slot, Hash);
type UpdateOperations = BTreeMap<(SlotHashKey, UpdateLabel), UpdateOperation>;

const MAX_ROOT_PRINT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(PartialEq, Eq, Clone, Debug, PartialOrd, Ord)]
enum UpdateLabel {
    Aggregate,
    Add,
    Subtract,
}

pub trait GetSlotHash {
    fn slot_hash(&self) -> SlotHashKey;
}

impl GetSlotHash for SlotHashKey {
    fn slot_hash(&self) -> SlotHashKey {
        *self
    }
}

impl GetSlotHash for Slot {
    fn slot_hash(&self) -> SlotHashKey {
        (*self, Hash::default())
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
enum UpdateOperation {
    Add(u64),
    Subtract(u64),
    Aggregate,
}

impl UpdateOperation {
    fn update_stake(&mut self, new_stake: u64) {
        match self {
            Self::Aggregate => panic!("Should not get here"),
            Self::Add(stake) => *stake += new_stake,
            Self::Subtract(stake) => *stake += new_stake,
        }
    }
}

#[derive(Clone, Debug)]
struct ForkInfo {
    // Amount of stake that has voted for exactly this slot
    stake_voted_at: ForkWeight,
    // Amount of stake that has voted for this slot and the subtree
    // rooted at this slot
    stake_voted_subtree: ForkWeight,
    // Tree height for the subtree rooted at this slot
    height: usize,
    // Best slot in the subtree rooted at this slot, does not
    // have to be a direct child in `children`. This is the slot whose subtree
    // is the heaviest.
    best_slot: SlotHashKey,
    // Deepest slot in the subtree rooted at this slot. This is the slot
    // with the greatest tree height.
    deepest_slot: SlotHashKey,
    parent: Option<SlotHashKey>,
    children: BTreeSet<SlotHashKey>,
}

#[cfg(test)]
impl PartialEq for ForkInfo {
    // Basic fork structure equality
    fn eq(&self, other: &Self) -> bool {
        self.parent == other.parent && self.children == other.children
    }
}

#[derive(Debug, Clone)]
pub struct HeaviestSubtreeForkChoice {
    fork_infos: AHashMap<SlotHashKey, ForkInfo>,
    latest_votes: HashMap<Pubkey, SlotHashKey>,
    tree_root: SlotHashKey,
    last_root_time: Instant,
}

#[cfg(test)]
impl PartialEq for HeaviestSubtreeForkChoice {
    // Basic fork structure equality
    fn eq(&self, other: &Self) -> bool {
        self.fork_infos == other.fork_infos && self.tree_root == other.tree_root
    }
}

#[cfg(test)]
impl PartialOrd for HeaviestSubtreeForkChoice {
    // Sort by root
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
impl Eq for HeaviestSubtreeForkChoice {}
#[cfg(test)]
impl Ord for HeaviestSubtreeForkChoice {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.tree_root.cmp(&other.tree_root)
    }
}

impl HeaviestSubtreeForkChoice {
    pub fn new(tree_root: SlotHashKey) -> Self {
        let mut heaviest_subtree_fork_choice = Self {
            tree_root,
            // Doesn't implement default because `root` must
            // exist in all the fields
            fork_infos: AHashMap::new(),
            latest_votes: HashMap::new(),
            last_root_time: Instant::now(),
        };
        heaviest_subtree_fork_choice.add_new_leaf_slot(tree_root, None);
        heaviest_subtree_fork_choice
    }

    #[cfg(test)]
    pub fn new_from_tree<T: GetSlotHash>(forks: Tree<T>) -> Self {
        let root = forks.root().data().slot_hash();
        let mut walk = TreeWalk::from(forks);
        let mut heaviest_subtree_fork_choice = HeaviestSubtreeForkChoice::new(root);
        while let Some(visit) = walk.get() {
            let slot_hash = visit.node().data().slot_hash();
            if heaviest_subtree_fork_choice
                .fork_infos
                .contains_key(&slot_hash)
            {
                walk.forward();
                continue;
            }
            let parent_slot_hash = walk.get_parent().map(|n| n.data().slot_hash());
            heaviest_subtree_fork_choice.add_new_leaf_slot(slot_hash, parent_slot_hash);
            walk.forward();
        }

        heaviest_subtree_fork_choice
    }

    pub fn contains_block(&self, key: &SlotHashKey) -> bool {
        self.fork_infos.contains_key(key)
    }

    pub fn best_slot(&self, key: &SlotHashKey) -> Option<SlotHashKey> {
        self.fork_infos
            .get(key)
            .map(|fork_info| fork_info.best_slot)
    }

    pub fn deepest_slot(&self, key: &SlotHashKey) -> Option<SlotHashKey> {
        self.fork_infos
            .get(key)
            .map(|fork_info| fork_info.deepest_slot)
    }

    pub fn best_overall_slot(&self) -> SlotHashKey {
        self.best_slot(&self.tree_root).unwrap()
    }

    pub fn deepest_overall_slot(&self) -> SlotHashKey {
        self.deepest_slot(&self.tree_root).unwrap()
    }

    pub fn stake_voted_subtree(&self, key: &SlotHashKey) -> Option<u64> {
        self.fork_infos
            .get(key)
            .map(|fork_info| fork_info.stake_voted_subtree)
    }

    pub fn height(&self, key: &SlotHashKey) -> Option<usize> {
        self.fork_infos.get(key).map(|fork_info| fork_info.height)
    }

    pub fn tree_root(&self) -> SlotHashKey {
        self.tree_root
    }

    pub fn max_by_weight(&self, slot1: SlotHashKey, slot2: SlotHashKey) -> std::cmp::Ordering {
        let weight1 = self.stake_voted_subtree(&slot1).unwrap();
        let weight2 = self.stake_voted_subtree(&slot2).unwrap();
        if weight1 == weight2 {
            slot1.cmp(&slot2).reverse()
        } else {
            weight1.cmp(&weight2)
        }
    }

    // Add new votes, returns the best slot
    pub fn add_votes<'a, 'b>(
        &'a mut self,
        // newly updated votes on a fork
        pubkey_votes: impl Iterator<Item = impl Borrow<(Pubkey, SlotHashKey)> + 'b>,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
    ) -> SlotHashKey {
        // Generate the set of updates
        let update_operations_batch =
            self.generate_update_operations(pubkey_votes, epoch_stakes, epoch_schedule);

        // Finalize all updates
        self.process_update_operations(update_operations_batch);
        self.best_overall_slot()
    }

    pub fn is_empty(&self) -> bool {
        self.fork_infos.is_empty()
    }

    pub fn set_tree_root(&mut self, new_root: SlotHashKey) {
        // Remove everything reachable from `self.tree_root` but not `new_root`,
        // as those are now unrooted.
        let remove_set = (&*self).subtree_diff(self.tree_root, new_root);
        for node_key in remove_set {
            self.fork_infos
                .remove(&node_key)
                .expect("Slots reachable from old root must exist in tree");
        }
        let root_fork_info = self.fork_infos.get_mut(&new_root);

        root_fork_info
            .unwrap_or_else(|| panic!("New root: {new_root:?}, didn't exist in fork choice"))
            .parent = None;
        self.tree_root = new_root;
        self.last_root_time = Instant::now();
    }

    /// Purges all slots < `new_root` and prunes subtrees with slots > `new_root` not descending from `new_root`.
    /// Also if the resulting tree is non-empty, updates `self.tree_root` to `new_root`
    /// Returns (purged slots, pruned subtrees)
    pub fn purge_prune(&mut self, new_root: SlotHashKey) -> (Vec<SlotHashKey>, Vec<Self>) {
        let mut pruned_subtrees = vec![];
        let mut purged_slots = vec![];
        let mut tree_root = None;
        // Find the subtrees to prune
        let mut to_visit = vec![self.tree_root];
        while let Some(cur_slot) = to_visit.pop() {
            if cur_slot == new_root {
                tree_root = Some(new_root);
                continue;
            }
            if cur_slot < new_root {
                for child in (&*self)
                    .children(&cur_slot)
                    .expect("slot was discovered earlier, must exist")
                {
                    to_visit.push(*child);
                }
                // Purge this slot since it's < `new_root`
                purged_slots.push(cur_slot);
            } else {
                // The start of a pruned subtree. Split it out and stop traversing this subtree.
                pruned_subtrees.push(self.split_off(&cur_slot));
            }
        }

        for slot in purged_slots.iter() {
            self.fork_infos
                .remove(slot)
                .expect("Slots reachable from old root must exist in tree");
        }
        if let Some(tree_root) = tree_root {
            self.fork_infos
                .get_mut(&tree_root)
                .expect("New tree_root must exist in fork_infos")
                .parent = None;
            self.tree_root = tree_root;
            self.last_root_time = Instant::now();
        }
        (purged_slots, pruned_subtrees)
    }

    pub fn add_root_parent(&mut self, root_parent: SlotHashKey) {
        assert!(root_parent.0 < self.tree_root.0);
        assert!(!self.fork_infos.contains_key(&root_parent));
        let root_info = self
            .fork_infos
            .get_mut(&self.tree_root)
            .expect("entry for root must exist");
        root_info.parent = Some(root_parent);
        let root_parent_info = ForkInfo {
            stake_voted_at: 0,
            stake_voted_subtree: root_info.stake_voted_subtree,
            height: root_info.height + 1,
            // The `best_slot` and `deepest_slot` do not change
            best_slot: root_info.best_slot,
            deepest_slot: root_info.deepest_slot,
            children: BTreeSet::from([self.tree_root]),
            parent: None,
        };
        self.fork_infos.insert(root_parent, root_parent_info);
        self.tree_root = root_parent;
    }

    pub fn maybe_print_state(&mut self) {
        if self.last_root_time.elapsed() > MAX_ROOT_PRINT_INTERVAL {
            self.print_state();
            self.last_root_time = Instant::now();
        }
    }

    pub fn add_new_leaf_slot(&mut self, slot_hash_key: SlotHashKey, parent: Option<SlotHashKey>) {
        if self.fork_infos.contains_key(&slot_hash_key) {
            // Can potentially happen if we repair the same version of the duplicate slot, after
            // dumping the original version
            return;
        }

        self.fork_infos
            .entry(slot_hash_key)
            .and_modify(|fork_info| fork_info.parent = parent)
            .or_insert(ForkInfo {
                stake_voted_at: 0,
                stake_voted_subtree: 0,
                height: 1,
                // The `best_slot` and `deepest_slot` of a leaf is itself
                best_slot: slot_hash_key,
                deepest_slot: slot_hash_key,
                children: BTreeSet::new(),
                parent,
            });

        if parent.is_none() {
            return;
        }

        let parent = parent.unwrap();

        // Parent must already exist by time child is being added
        self.fork_infos
            .get_mut(&parent)
            .unwrap()
            .children
            .insert(slot_hash_key);

        // Propagate leaf up the tree to any ancestors who considered the previous leaf
        // the `best_slot`, as well as any deepest slot info
        self.propagate_new_leaf(&slot_hash_key, &parent);
    }

    // Returns true if the given `maybe_best_child` is the heaviest among the children
    // of the parent. Breaks ties by slot # (lower is heavier).
    fn is_best_child(&self, maybe_best_child: &SlotHashKey) -> bool {
        let maybe_best_child_weight = self.stake_voted_subtree(maybe_best_child).unwrap();
        let parent = self.parent(maybe_best_child);
        // If there's no parent, this must be the root
        if parent.is_none() {
            return true;
        }
        for child in self.children(&parent.unwrap()).unwrap() {
            let child_weight = self
                .stake_voted_subtree(child)
                .expect("child must exist in `self.fork_infos`");

            if child_weight > maybe_best_child_weight
                || (maybe_best_child_weight == child_weight && *child < *maybe_best_child)
            {
                return false;
            }
        }

        true
    }

    // Returns true if the given `maybe_deepest_child` is the deepest among the children
    // of the parent. Breaks ties by stake, then slot # (lower is heavier).
    fn is_deepest_child(&self, maybe_deepest_child: &SlotHashKey) -> bool {
        let maybe_deepest_child_weight = self.stake_voted_subtree(maybe_deepest_child).unwrap();
        let maybe_deepest_child_height = self.height(maybe_deepest_child).unwrap();
        let parent = self.parent(maybe_deepest_child);
        // If there's no parent, this must be the root
        if parent.is_none() {
            return true;
        }
        for child in self.children(&parent.unwrap()).unwrap() {
            let child_height = self
                .height(child)
                .expect("child must exist in `self.fork_infos`");
            let child_weight = self
                .stake_voted_subtree(child)
                .expect("child must exist in `self.fork_infos`");

            match (
                child_height.cmp(&maybe_deepest_child_height),
                child_weight.cmp(&maybe_deepest_child_weight),
                child.cmp(maybe_deepest_child),
            ) {
                (Ordering::Greater, _, _) => return false,
                // Tiebreak by stake
                (Ordering::Equal, Ordering::Greater, _) => return false,
                // Tiebreak by slot #
                (Ordering::Equal, Ordering::Equal, Ordering::Less) => return false,
                _ => (),
            }
        }

        true
    }

    pub fn all_slots_stake_voted_subtree(&self) -> impl Iterator<Item = (&SlotHashKey, u64)> {
        self.fork_infos
            .iter()
            .map(|(slot_hash, fork_info)| (slot_hash, fork_info.stake_voted_subtree))
    }

    pub fn slots_iter(&self) -> impl Iterator<Item = Slot> + '_ {
        self.fork_infos.iter().map(|((slot, _), _)| slot).copied()
    }

    /// Split off the node at `slot_hash_key` and propagate the stake subtraction up to the root of the
    /// tree.
    ///
    /// Assumes that `slot_hash_key` is not the `tree_root`
    /// Returns the subtree originating from `slot_hash_key`
    pub fn split_off(&mut self, slot_hash_key: &SlotHashKey) -> Self {
        assert_ne!(self.tree_root, *slot_hash_key);
        let (mut split_tree_root, parent) = {
            let node_to_split_at = self
                .fork_infos
                .get_mut(slot_hash_key)
                .expect("Slot hash key must exist in tree");
            (
                node_to_split_at.clone(),
                node_to_split_at
                    .parent
                    .expect("Split node is not tree root"),
            )
        };

        let mut update_operations: UpdateOperations = BTreeMap::new();
        // Insert aggregate operations up to the root
        self.insert_aggregate_operations(&mut update_operations, *slot_hash_key);
        // Remove child link so that this slot cannot be chosen as best or deepest
        assert!(
            self.fork_infos
                .get_mut(&parent)
                .expect("Parent must exist in fork_infos")
                .children
                .remove(slot_hash_key)
        );
        // Aggregate
        self.process_update_operations(update_operations);

        // Remove node + all children and add to new tree
        let mut split_tree_fork_infos = AHashMap::new();
        let mut to_visit = vec![*slot_hash_key];
        while let Some(current_node) = to_visit.pop() {
            let current_fork_info = self
                .fork_infos
                .remove(&current_node)
                .expect("Node must exist in tree");

            to_visit.extend(current_fork_info.children.iter());
            split_tree_fork_infos.insert(current_node, current_fork_info);
        }

        // Remove link from parent
        let parent_fork_info = self
            .fork_infos
            .get_mut(&split_tree_root.parent.expect("Cannot split off from root"))
            .expect("Parent must exist in fork infos");
        parent_fork_info.children.remove(slot_hash_key);

        // Update the root of the new tree with the proper info, now that we have finished
        // aggregating
        split_tree_root.parent = None;
        split_tree_fork_infos.insert(*slot_hash_key, split_tree_root);

        // Split off the relevant votes to the new tree
        let mut split_tree_latest_votes = self.latest_votes.clone();
        split_tree_latest_votes.retain(|_, node| split_tree_fork_infos.contains_key(node));
        self.latest_votes
            .retain(|_, node| self.fork_infos.contains_key(node));

        // Create a new tree from the split
        HeaviestSubtreeForkChoice {
            tree_root: *slot_hash_key,
            fork_infos: split_tree_fork_infos,
            latest_votes: split_tree_latest_votes,
            last_root_time: Instant::now(),
        }
    }

    #[cfg(test)]
    pub fn ancestors(&self, start_slot_hash_key: SlotHashKey) -> Vec<SlotHashKey> {
        AncestorIterator::new(start_slot_hash_key, &self.fork_infos).collect()
    }

    pub fn merge(
        &mut self,
        other: HeaviestSubtreeForkChoice,
        merge_leaf: &SlotHashKey,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
    ) {
        assert!(self.fork_infos.contains_key(merge_leaf));

        // Add all the nodes from `other` into our tree
        let mut other_slots_nodes: Vec<_> = other
            .fork_infos
            .iter()
            .map(|(slot_hash_key, fork_info)| {
                (slot_hash_key, fork_info.parent.unwrap_or(*merge_leaf))
            })
            .collect();

        other_slots_nodes.sort_by_key(|(slot_hash_key, _)| *slot_hash_key);
        for (slot_hash_key, parent) in other_slots_nodes {
            self.add_new_leaf_slot(*slot_hash_key, Some(parent));
        }

        // Add all votes, the outdated ones should be filtered out by
        // self.add_votes()
        self.add_votes(other.latest_votes.into_iter(), epoch_stakes, epoch_schedule);
    }

    pub fn stake_voted_at(&self, slot: &SlotHashKey) -> Option<u64> {
        self.fork_infos
            .get(slot)
            .map(|fork_info| fork_info.stake_voted_at)
    }

    /// Returns if a node with slot `maybe_ancestor_slot` is an ancestor of the node with
    /// key `node_key`
    pub fn is_strict_ancestor(
        &self,
        maybe_ancestor_key: &SlotHashKey,
        node_key: &SlotHashKey,
    ) -> bool {
        if maybe_ancestor_key == node_key {
            return false;
        }

        if maybe_ancestor_key.0 > node_key.0 {
            return false;
        }

        let mut ancestor_iterator = self.ancestor_iterator(*node_key);
        ancestor_iterator.any(|(ancestor_slot, ancestor_hash)| {
            ancestor_slot == maybe_ancestor_key.0 && ancestor_hash == maybe_ancestor_key.1
        })
    }

    /// To be called when `slot_hash_key` has been added to `self.fork_infos`, before any
    /// aggregate update operations have taken place.
    ///
    /// Will propagate update `best_slot` and `deepest_slot` to ancestors.
    fn propagate_new_leaf(
        &mut self,
        slot_hash_key: &SlotHashKey,
        parent_slot_hash_key: &SlotHashKey,
    ) {
        let parent_best_slot_hash_key = self
            .best_slot(parent_slot_hash_key)
            .expect("parent must exist in self.fork_infos after its child leaf was created");
        // If this new leaf is the direct parent's best child, then propagate it up the tree
        if self.is_best_child(slot_hash_key) {
            let mut ancestor = Some(*parent_slot_hash_key);
            loop {
                if ancestor.is_none() {
                    break;
                }
                let ancestor_fork_info = self.fork_infos.get_mut(&ancestor.unwrap()).unwrap();
                if ancestor_fork_info.best_slot == parent_best_slot_hash_key {
                    ancestor_fork_info.best_slot = *slot_hash_key;
                } else {
                    break;
                }
                ancestor = ancestor_fork_info.parent;
            }
        }
        // Propagate the deepest slot up the tree
        let mut ancestor = Some(*parent_slot_hash_key);
        let mut current_child = *slot_hash_key;
        let mut current_height = 1;
        loop {
            if ancestor.is_none() {
                break;
            }
            if !self.is_deepest_child(&current_child) {
                break;
            }
            let ancestor_fork_info = self.fork_infos.get_mut(&ancestor.unwrap()).unwrap();
            ancestor_fork_info.deepest_slot = *slot_hash_key;
            ancestor_fork_info.height = current_height + 1;
            current_child = ancestor.unwrap();
            current_height = ancestor_fork_info.height;
            ancestor = ancestor_fork_info.parent;
        }
    }

    fn insert_aggregate_operations(
        &self,
        update_operations: &mut BTreeMap<(SlotHashKey, UpdateLabel), UpdateOperation>,
        slot_hash_key: SlotHashKey,
    ) {
        self.do_insert_aggregate_operations_across_ancestors(update_operations, slot_hash_key);
    }

    #[allow(clippy::map_entry)]
    fn do_insert_aggregate_operations_across_ancestors(
        &self,
        update_operations: &mut BTreeMap<(SlotHashKey, UpdateLabel), UpdateOperation>,
        slot_hash_key: SlotHashKey,
    ) {
        for parent_slot_hash_key in self.ancestor_iterator(slot_hash_key) {
            if !self.do_insert_aggregate_operation(update_operations, parent_slot_hash_key) {
                // If this parent was already inserted, we assume all the other parents have also
                // already been inserted. This is to prevent iterating over the parents multiple times
                // when we are aggregating leaves that have a lot of shared ancestors
                break;
            }
        }
    }

    #[allow(clippy::map_entry)]
    fn do_insert_aggregate_operation(
        &self,
        update_operations: &mut BTreeMap<(SlotHashKey, UpdateLabel), UpdateOperation>,
        slot_hash_key: SlotHashKey,
    ) -> bool {
        let aggregate_label = (slot_hash_key, UpdateLabel::Aggregate);
        if update_operations.contains_key(&aggregate_label) {
            false
        } else {
            update_operations.insert(aggregate_label, UpdateOperation::Aggregate);
            true
        }
    }

    fn ancestor_iterator(&self, start_slot_hash_key: SlotHashKey) -> AncestorIterator<'_> {
        AncestorIterator::new(start_slot_hash_key, &self.fork_infos)
    }

    fn aggregate_slot(&mut self, slot_hash_key: SlotHashKey) {
        let mut stake_voted_subtree;
        let mut deepest_child_height = 0;
        let mut best_slot_hash_key = slot_hash_key;
        let mut deepest_slot_hash_key = slot_hash_key;
        if let Some(fork_info) = self.fork_infos.get(&slot_hash_key) {
            stake_voted_subtree = fork_info.stake_voted_at;
            let mut best_child_stake_voted_subtree = 0;
            let mut best_child_slot_key = slot_hash_key;
            let mut deepest_child_stake_voted_subtree = 0;
            let mut deepest_child_slot_key = slot_hash_key;
            for child_key in &fork_info.children {
                let child_fork_info = self
                    .fork_infos
                    .get(child_key)
                    .expect("Child must exist in fork_info map");
                let child_stake_voted_subtree = child_fork_info.stake_voted_subtree;
                let child_height = child_fork_info.height;
                stake_voted_subtree += child_stake_voted_subtree;

                if best_child_slot_key == slot_hash_key ||
                    child_stake_voted_subtree > best_child_stake_voted_subtree ||
                // tiebreaker by slot height, prioritize earlier slot
                (child_stake_voted_subtree == best_child_stake_voted_subtree && child_key < &best_child_slot_key)
                {
                    best_child_stake_voted_subtree = child_stake_voted_subtree;
                    best_child_slot_key = *child_key;
                    best_slot_hash_key = child_fork_info.best_slot;
                }

                match (
                    deepest_child_slot_key == slot_hash_key,
                    child_height.cmp(&deepest_child_height),
                    child_stake_voted_subtree.cmp(&deepest_child_stake_voted_subtree),
                    child_key.cmp(&deepest_child_slot_key)
                ) {
                    // First child
                    (true, _, _, _) |
                    // or deeper child
                    (_, Ordering::Greater, _, _) |
                    // or tie break by stake weight
                    (_, Ordering::Equal, Ordering::Greater, _) |
                    // or tie break by slot #
                    (_, Ordering::Equal, Ordering::Equal, Ordering::Less) => {
                        deepest_child_height = child_height;
                        deepest_child_stake_voted_subtree = child_stake_voted_subtree;
                        deepest_child_slot_key = *child_key;
                        deepest_slot_hash_key = child_fork_info.deepest_slot;
                    },
                    _ => ()
                }
            }
        } else {
            return;
        }

        let fork_info = self.fork_infos.get_mut(&slot_hash_key).unwrap();
        fork_info.stake_voted_subtree = stake_voted_subtree;
        fork_info.height = deepest_child_height + 1;
        fork_info.best_slot = best_slot_hash_key;
        fork_info.deepest_slot = deepest_slot_hash_key;
    }

    fn generate_update_operations<'a, 'b>(
        &'a mut self,
        pubkey_votes: impl Iterator<Item = impl Borrow<(Pubkey, SlotHashKey)> + 'b>,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
    ) -> UpdateOperations {
        let mut update_operations: BTreeMap<(SlotHashKey, UpdateLabel), UpdateOperation> =
            BTreeMap::new();
        let mut observed_pubkeys: HashMap<Pubkey, Slot> = HashMap::new();
        // Sort the `pubkey_votes` in a BTreeMap by the slot voted
        for pubkey_vote in pubkey_votes {
            let (pubkey, new_vote_slot_hash) = pubkey_vote.borrow();
            let (new_vote_slot, new_vote_hash) = *new_vote_slot_hash;
            if new_vote_slot < self.tree_root.0 {
                // If the new vote is less than the root we can ignore it. This is because there
                // are two cases. Either:
                // 1) The validator's latest vote was bigger than the new vote, so we can ignore it
                // 2) The validator's latest vote was less than the new vote, then the validator's latest
                // vote was also less than root. This means either every node in the current tree has the
                // validators stake counted toward it (if the latest vote was an ancestor of the current root),
                // OR every node doesn't have this validator's vote counting toward it (if the latest vote
                // was not an ancestor of the current root). Thus this validator is essentially a no-op
                // and won't affect fork choice.
                continue;
            }

            // A pubkey cannot appear in pubkey votes more than once.
            match observed_pubkeys.entry(*pubkey) {
                Entry::Occupied(_occupied_entry) => {
                    panic!("Should not get multiple votes for same pubkey in the same batch");
                }
                Entry::Vacant(vacant_entry) => {
                    vacant_entry.insert(new_vote_slot);
                    false
                }
            };

            let mut pubkey_latest_vote = self.latest_votes.get_mut(pubkey);

            // Filter out any votes or slots < any slot this pubkey has
            // already voted for, we only care about the latest votes.
            //
            // If the new vote is for the same slot, but a different, smaller hash,
            // then allow processing to continue as this is a duplicate version
            // of the same slot.
            match pubkey_latest_vote.as_mut() {
                Some((pubkey_latest_vote_slot, pubkey_latest_vote_hash))
                    if (new_vote_slot < *pubkey_latest_vote_slot)
                        || (new_vote_slot == *pubkey_latest_vote_slot
                            && &new_vote_hash >= pubkey_latest_vote_hash) =>
                {
                    continue;
                }

                _ => {
                    // We either:
                    // 1) don't have a vote yet for this pubkey,
                    // 2) or the new vote slot is bigger than the old vote slot
                    // 3) or the new vote slot == old_vote slot, but for a smaller bank hash.
                    // In all above cases, we need to remove this pubkey stake from the previous fork
                    // of the previous vote

                    if let Some((old_latest_vote_slot, old_latest_vote_hash)) =
                        self.latest_votes.insert(*pubkey, *new_vote_slot_hash)
                    {
                        assert!(if new_vote_slot == old_latest_vote_slot {
                            warn!(
                                "Got a duplicate vote for validator: {pubkey}, slot_hash: \
                                 {new_vote_slot_hash:?}",
                            );
                            // If the slots are equal, then the new
                            // vote must be for a smaller hash
                            new_vote_hash < old_latest_vote_hash
                        } else {
                            new_vote_slot > old_latest_vote_slot
                        });

                        let epoch = epoch_schedule.get_epoch(old_latest_vote_slot);
                        let stake_update = epoch_stakes
                            .get(&epoch)
                            .map(|epoch_stakes| epoch_stakes.vote_account_stake(pubkey))
                            .unwrap_or(0);

                        if stake_update > 0 {
                            update_operations
                                .entry((
                                    (old_latest_vote_slot, old_latest_vote_hash),
                                    UpdateLabel::Subtract,
                                ))
                                .and_modify(|update| update.update_stake(stake_update))
                                .or_insert(UpdateOperation::Subtract(stake_update));
                            self.insert_aggregate_operations(
                                &mut update_operations,
                                (old_latest_vote_slot, old_latest_vote_hash),
                            );
                        }
                    }
                }
            }

            // Add this pubkey stake to new fork
            let epoch = epoch_schedule.get_epoch(new_vote_slot_hash.0);
            let stake_update = epoch_stakes
                .get(&epoch)
                .map(|epoch_stakes| epoch_stakes.vote_account_stake(pubkey))
                .unwrap_or(0);

            update_operations
                .entry((*new_vote_slot_hash, UpdateLabel::Add))
                .and_modify(|update| update.update_stake(stake_update))
                .or_insert(UpdateOperation::Add(stake_update));
            self.insert_aggregate_operations(&mut update_operations, *new_vote_slot_hash);
        }

        update_operations
    }

    fn process_update_operations(&mut self, update_operations: UpdateOperations) {
        // Iterate through the update operations from greatest to smallest slot
        for ((slot_hash_key, _), operation) in update_operations.into_iter().rev() {
            match operation {
                UpdateOperation::Aggregate => self.aggregate_slot(slot_hash_key),
                UpdateOperation::Add(stake) => self.add_slot_stake(&slot_hash_key, stake),
                UpdateOperation::Subtract(stake) => self.subtract_slot_stake(&slot_hash_key, stake),
            }
        }
    }

    fn add_slot_stake(&mut self, slot_hash_key: &SlotHashKey, stake: u64) {
        if let Some(fork_info) = self.fork_infos.get_mut(slot_hash_key) {
            fork_info.stake_voted_at += stake;
            fork_info.stake_voted_subtree += stake;
        }
    }

    fn subtract_slot_stake(&mut self, slot_hash_key: &SlotHashKey, stake: u64) {
        if let Some(fork_info) = self.fork_infos.get_mut(slot_hash_key) {
            fork_info.stake_voted_at -= stake;
            fork_info.stake_voted_subtree -= stake;
        }
    }

    fn parent(&self, slot_hash_key: &SlotHashKey) -> Option<SlotHashKey> {
        self.fork_infos
            .get(slot_hash_key)
            .map(|fork_info| fork_info.parent)
            .unwrap_or(None)
    }

    fn print_state(&self) {
        let best_slot_hash_key = self.best_overall_slot();
        let mut best_path: VecDeque<_> = self.ancestor_iterator(best_slot_hash_key).collect();
        best_path.push_front(best_slot_hash_key);
        info!(
            "Latest known votes by vote pubkey: {:#?}, best path: {:?}",
            self.latest_votes,
            best_path.iter().rev().collect::<Vec<&SlotHashKey>>()
        );
    }
}

impl<'a> TreeDiff<'a> for &'a HeaviestSubtreeForkChoice {
    type TreeKey = SlotHashKey;
    type ChildIter = Iter<'a, SlotHashKey>;

    fn contains_slot(&self, slot_hash_key: &SlotHashKey) -> bool {
        self.fork_infos.contains_key(slot_hash_key)
    }

    fn children(&self, slot_hash_key: &SlotHashKey) -> Option<Self::ChildIter> {
        self.fork_infos
            .get(slot_hash_key)
            .map(|fork_info| fork_info.children.iter())
    }
}

struct AncestorIterator<'a> {
    current_slot_hash_key: SlotHashKey,
    fork_infos: &'a AHashMap<SlotHashKey, ForkInfo>,
}

impl<'a> AncestorIterator<'a> {
    fn new(
        start_slot_hash_key: SlotHashKey,
        fork_infos: &'a AHashMap<SlotHashKey, ForkInfo>,
    ) -> Self {
        Self {
            current_slot_hash_key: start_slot_hash_key,
            fork_infos,
        }
    }
}

impl Iterator for AncestorIterator<'_> {
    type Item = SlotHashKey;
    fn next(&mut self) -> Option<Self::Item> {
        let parent_slot_hash_key = self
            .fork_infos
            .get(&self.current_slot_hash_key)
            .map(|fork_info| fork_info.parent)
            .unwrap_or(None);

        parent_slot_hash_key
            .map(|parent_slot_hash_key| {
                self.current_slot_hash_key = parent_slot_hash_key;
                Some(self.current_slot_hash_key)
            })
            .unwrap_or(None)
    }
}
