use {
    crate::{
        consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
        repair::{
            repair_generic_traversal::{get_closest_completion, get_unknown_last_index},
            repair_service::{RepairEligibility, RepairMetrics, RepairService},
            repair_weighted_traversal,
            serve_repair::ShredRepairType,
        },
    },
    ahash::{AHashMap, AHashSet},
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_ledger::{
        ancestor_iterator::AncestorIterator,
        blockstore::Blockstore,
        blockstore_db::DBPinnableSlice,
        blockstore_meta::{NextSlots, SlotMetaRepair},
    },
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::VersionedEpochStakes,
    std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
};

#[derive(PartialEq, Eq, Copy, Clone, Hash, Debug)]
enum TreeRoot {
    Root(Slot),
}

impl From<TreeRoot> for Slot {
    fn from(val: TreeRoot) -> Self {
        match val {
            TreeRoot::Root(slot) => slot,
        }
    }
}

#[derive(Clone)]
pub struct RepairWeight {
    // Map from root -> a subtree rooted at that `root`
    trees: HashMap<Slot, HeaviestSubtreeForkChoice>,
    // Maps each slot to the root of the tree that contains it
    slot_to_tree: HashMap<Slot, TreeRoot>,
    root: Slot,
}

impl RepairWeight {
    pub fn new(root: Slot) -> Self {
        let root_tree = HeaviestSubtreeForkChoice::new((root, Hash::default()));
        let slot_to_tree = HashMap::from([(root, TreeRoot::Root(root))]);
        let trees = HashMap::from([(root, root_tree)]);
        Self {
            trees,
            slot_to_tree,
            root,
        }
    }

    pub fn add_voters(
        &mut self,
        blockstore: &Blockstore,
        voters: impl Iterator<Item = (Slot, Vec<Pubkey>)>,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
    ) {
        let mut all_subtree_updates: HashMap<TreeRoot, HashMap<Pubkey, Slot>> = HashMap::new();
        for (slot, pubkey_voters) in voters {
            if slot < self.root {
                continue;
            }
            let mut tree_root = self.get_tree_root(slot);
            let mut new_ancestors = VecDeque::new();
            // If we do not know how this slot chains to an existing tree, use blockstore to
            // discover its ancestors.
            if tree_root.is_none() {
                let (discovered_ancestors, existing_subtree_root) =
                    self.find_ancestor_subtree_of_slot(blockstore, slot);
                new_ancestors = discovered_ancestors;
                tree_root = existing_subtree_root;
            }

            let (tree_root, tree) = {
                match (tree_root, *new_ancestors.front().unwrap_or(&slot)) {
                    (Some(tree_root), _) => (
                        tree_root,
                        self.trees
                            .get_mut(&tree_root.into())
                            .expect("If tree root was found, it must exist in `self.trees`"),
                    ),
                    (None, earliest_ancestor) => {
                        // There is no known subtree that contains `slot`. Thus, create a new
                        // subtree rooted at the earliest known ancestor of `slot`.
                        if earliest_ancestor < self.root {
                            continue;
                        } else {
                            // We guarantee that `earliest_ancestor` does not already exist in
                            // `self.trees` otherwise `tree_root` would not be None
                            self.insert_new_tree(earliest_ancestor);
                            (
                                TreeRoot::Root(earliest_ancestor),
                                self.trees.get_mut(&earliest_ancestor).unwrap(),
                            )
                        }
                    }
                }
            };

            // First element in `ancestors` must be either:
            // 1) Leaf of some existing subtree
            // 2) Root of a new subtree created above through `self.insert_new_tree`
            new_ancestors.push_back(slot);
            if new_ancestors.len() > 1 {
                for i in 0..new_ancestors.len() - 1 {
                    // TODO: Repair right now does not distinguish between votes for different
                    // versions of the same slot.
                    tree.add_new_leaf_slot(
                        (new_ancestors[i + 1], Hash::default()),
                        Some((new_ancestors[i], Hash::default())),
                    );
                    self.slot_to_tree.insert(new_ancestors[i + 1], tree_root);
                }
            }

            // Now we know which subtree this slot chains to,
            // add the votes to the list of updates
            let subtree_updates = all_subtree_updates.entry(tree_root).or_default();
            for pubkey in pubkey_voters {
                let cur_max = subtree_updates.entry(pubkey).or_default();
                *cur_max = std::cmp::max(*cur_max, slot);
            }
        }

        for (tree_root, updates) in all_subtree_updates {
            let tree = self
                .get_tree_mut(tree_root)
                .expect("Tree for `tree_root` must exist here");
            let updates: Vec<_> = updates.into_iter().collect();
            tree.add_votes(
                updates
                    .iter()
                    .map(|(pubkey, slot)| (*pubkey, (*slot, Hash::default()))),
                epoch_stakes,
                epoch_schedule,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn get_best_weighted_repairs<'db>(
        &mut self,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
        max_new_orphans: usize,
        max_new_shreds: usize,
        max_unknown_last_index_repairs: usize,
        max_closest_completion_repairs: usize,
        repair_eligibility: &mut RepairEligibility,
        repair_metrics: &mut RepairMetrics,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
        full_slots_cache: &mut AHashMap<Slot, NextSlots>,
    ) -> Vec<ShredRepairType> {
        let mut repairs = vec![];
        let mut processed_slots = AHashSet::from([self.root]);
        let mut slot_meta_cache = AHashMap::default();

        let mut get_best_orphans_us = Measure::start("get_best_orphans_us");
        // Find the best orphans in order from heaviest stake to least heavy
        self.get_best_orphans(
            blockstore,
            &mut processed_slots,
            &mut repairs,
            epoch_stakes,
            epoch_schedule,
            max_new_orphans,
            outstanding_repairs,
        );
        // Subtract 1 because the root is not processed as an orphan
        let num_orphan_slots = processed_slots.len() - 1;
        let num_orphan_repairs = repairs.len();
        get_best_orphans_us.stop();

        let mut get_best_shreds_us = Measure::start("get_best_shreds_us");
        let mut best_shreds_repairs = Vec::default();
        // Find the best incomplete slots in rooted subtree
        let (cache_hits, cache_misses) = self.get_best_shreds(
            blockstore,
            pinnable_slice,
            &mut slot_meta_cache,
            full_slots_cache,
            &mut best_shreds_repairs,
            max_new_shreds,
            repair_eligibility,
            outstanding_repairs,
        );
        repair_metrics.best_repairs_stats.weighted_shreds_cache_hits += cache_hits;
        repair_metrics
            .best_repairs_stats
            .weighted_shreds_cache_misses += cache_misses;
        let num_best_shreds_repairs = best_shreds_repairs.len();
        let repair_slots_set: HashSet<Slot> =
            best_shreds_repairs.iter().map(|r| r.slot()).collect();
        let num_best_shreds_slots = repair_slots_set.len();
        processed_slots.extend(repair_slots_set);
        repairs.extend(best_shreds_repairs);
        get_best_shreds_us.stop();

        // Although we have generated repairs for orphan roots and slots in the rooted subtree,
        // if we have space we should generate repairs for slots in orphan trees in preparation for
        // when they are no longer rooted. Here we generate repairs for slots with unknown last
        // indices as well as slots that are close to completion.

        let mut get_unknown_last_index_us = Measure::start("get_unknown_last_index_us");
        let pre_num_slots = processed_slots.len();
        let unknown_last_index_repairs = self.get_best_unknown_last_index(
            blockstore,
            pinnable_slice,
            &mut slot_meta_cache,
            full_slots_cache,
            &mut processed_slots,
            max_unknown_last_index_repairs,
            outstanding_repairs,
        );
        let num_unknown_last_index_repairs = unknown_last_index_repairs.len();
        let num_unknown_last_index_slots = processed_slots.len() - pre_num_slots;
        repairs.extend(unknown_last_index_repairs);
        get_unknown_last_index_us.stop();

        let mut get_closest_completion_us = Measure::start("get_closest_completion_us");
        let pre_num_slots = processed_slots.len();
        let (closest_completion_repairs, total_slots_processed) = self.get_best_closest_completion(
            blockstore,
            pinnable_slice,
            &mut slot_meta_cache,
            full_slots_cache,
            &mut processed_slots,
            max_closest_completion_repairs,
            repair_eligibility,
            outstanding_repairs,
        );
        let num_closest_completion_repairs = closest_completion_repairs.len();
        let num_closest_completion_slots = processed_slots.len() - pre_num_slots;
        let num_closest_completion_slots_path =
            total_slots_processed.saturating_sub(num_closest_completion_slots);
        repairs.extend(closest_completion_repairs);
        get_closest_completion_us.stop();

        // Preserve only the stable part of the per-iteration metadata cache. Incomplete metadata
        // must be fetched again because new shreds can change it at any time.
        for (slot, slot_meta) in &slot_meta_cache {
            if let Some(slot_meta) = slot_meta
                && slot_meta.is_full()
                && let Entry::Vacant(entry) = full_slots_cache.entry(*slot)
            {
                entry.insert(slot_meta.next_slots.clone());
            }
        }
        repair_metrics.best_repairs_stats.update(
            num_orphan_slots as u64,
            num_orphan_repairs as u64,
            num_best_shreds_slots as u64,
            num_best_shreds_repairs as u64,
            num_unknown_last_index_slots as u64,
            num_unknown_last_index_repairs as u64,
            num_closest_completion_slots as u64,
            num_closest_completion_slots_path as u64,
            num_closest_completion_repairs as u64,
            self.trees.len() as u64,
        );
        repair_metrics.timing.get_best_orphans_us += get_best_orphans_us.as_us();
        repair_metrics.timing.get_best_shreds_us += get_best_shreds_us.as_us();
        repair_metrics.timing.get_unknown_last_index_us += get_unknown_last_index_us.as_us();
        repair_metrics.timing.get_closest_completion_us += get_closest_completion_us.as_us();

        repairs
    }

    /// Split `slot` and descendants into an orphan tree in repair weighting.
    ///
    /// If repair holds a subtree `ST` which contains `slot`, we split `ST` into `(T, T')` where `T'` is
    /// a subtree rooted at `slot` and `T` is what remains (if anything) of `ST` after the split.
    /// If `T` is non-empty, it remains in `self.trees`; `T'` is inserted as another tree.
    /// This function removes and destroys the original `ST`.
    ///
    /// Assumes that `slot` is greater than `self.root`.
    /// Returns slots that were orphaned
    pub fn split_off(&mut self, slot: Slot) -> HashSet<Slot> {
        assert!(slot >= self.root);
        if slot == self.root {
            error!("Trying to orphan root of repair tree {slot}");
            return HashSet::new();
        }
        match self.slot_to_tree.get(&slot).copied() {
            Some(TreeRoot::Root(subtree_root)) => {
                if subtree_root == slot {
                    info!("{slot} is already orphan, skipping");
                    return HashSet::new();
                }
                let subtree = self
                    .trees
                    .get_mut(&subtree_root)
                    .expect("`self.slot_to_tree` and `self.trees` must be in sync");
                let orphaned_tree = subtree.split_off(&(slot, Hash::default()));
                self.rename_tree_root(&orphaned_tree, TreeRoot::Root(slot));
                self.trees.insert(slot, orphaned_tree);
                self.trees.get(&slot).unwrap().slots_iter().collect()
            }
            None => {
                warn!("Trying to split off slot {slot} which doesn't currently exist in repair");
                HashSet::new()
            }
        }
    }

    pub fn set_root(&mut self, new_root: Slot) {
        // Roots should be monotonically increasing
        assert!(self.root <= new_root);

        if new_root == self.root {
            return;
        }

        // Root slot of the tree that contains `new_root`, if one exists
        let new_root_tree_root = self.slot_to_tree.get(&new_root).copied().map(Into::into);

        // Purge outdated trees from `self.trees`
        // These are all subtrees that have a `tree_root` < `new_root` and are not part of the
        // rooted subtree
        let subtrees_to_purge: Vec<_> = self
            .trees
            .keys()
            .filter(|subtree_root| {
                **subtree_root < new_root
                    && new_root_tree_root
                        .map(|new_root_tree_root| **subtree_root != new_root_tree_root)
                        .unwrap_or(true)
            })
            .copied()
            .collect();
        for subtree_root in subtrees_to_purge {
            let subtree = self
                .trees
                .remove(&subtree_root)
                .expect("Must exist, was found in `self.trees` above");

            self.remove_tree_slots(&subtree);
        }

        if let Some(new_root_tree_root) = new_root_tree_root {
            let mut new_root_tree = self
                .trees
                .remove(&new_root_tree_root)
                .expect("Found slot root earlier in self.slot_to_trees, tree must exist");

            // Find and discard descendants of `self.root` that are not reachable from `new_root`.
            trace!("pruning tree {new_root_tree_root} with {new_root}");
            let (removed, pruned) = new_root_tree.purge_prune((new_root, Hash::default()));
            for pruned_tree in pruned {
                self.remove_tree_slots(&pruned_tree);
            }

            for (slot, _) in removed {
                self.slot_to_tree.remove(&slot);
            }

            // Update `self.slot_to_tree` to reflect new root
            new_root_tree.set_tree_root((new_root, Hash::default()));
            self.rename_tree_root(&new_root_tree, TreeRoot::Root(new_root));

            // Insert the tree for the new root
            self.trees.insert(new_root, new_root_tree);
        } else {
            // Guaranteed that `new_root` is not part of `self.trees` because `new_root_tree_root`
            // is None
            self.insert_new_tree(new_root);
        }

        self.root = new_root;
    }

    pub fn root(&self) -> Slot {
        self.root
    }

    // Generate shred repairs for main subtree rooted at `self.root`
    #[allow(clippy::too_many_arguments)]
    fn get_best_shreds<'db>(
        &mut self,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        slot_meta_cache: &mut AHashMap<Slot, Option<SlotMetaRepair>>,
        full_slots_cache: &mut AHashMap<Slot, NextSlots>,
        repairs: &mut Vec<ShredRepairType>,
        max_new_shreds: usize,
        repair_eligibility: &mut RepairEligibility,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> (u64, u64) {
        let root_tree = self.trees.get(&self.root).expect("Root tree must exist");
        repair_weighted_traversal::get_best_repair_shreds(
            root_tree,
            blockstore,
            pinnable_slice,
            slot_meta_cache,
            full_slots_cache,
            repairs,
            max_new_shreds,
            repair_eligibility,
            outstanding_repairs,
        )
    }

    fn get_best_orphans(
        &mut self,
        blockstore: &Blockstore,
        processed_slots: &mut AHashSet<Slot>,
        repairs: &mut Vec<ShredRepairType>,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
        max_new_orphans: usize,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) {
        // Sort each tree in `self.trees`, by the amount of stake that has voted on each,
        // tiebreaker going to earlier slots, thus prioritizing earlier slots on the same fork
        // to ensure replay can continue as soon as possible.
        let mut stake_weighted_trees: Vec<(Slot, u64)> = self
            .trees
            .iter()
            .map(|(slot, tree)| {
                (
                    *slot,
                    tree.stake_voted_subtree(&(*slot, Hash::default()))
                        .expect("Tree must have weight at its own root"),
                )
            })
            .collect();

        // Heavier, smaller slots come first
        Self::sort_by_stake_weight_slot(&mut stake_weighted_trees);
        let mut new_best_orphan_requests = 0;
        for (heaviest_tree_root, _) in stake_weighted_trees {
            if new_best_orphan_requests >= max_new_orphans {
                break;
            }
            if processed_slots.contains(&heaviest_tree_root) {
                continue;
            }
            // Ignore trees that were merged in a previous iteration
            if self.trees.contains_key(&heaviest_tree_root) {
                let new_orphan_root = self.update_orphan_ancestors(
                    blockstore,
                    heaviest_tree_root,
                    epoch_stakes,
                    epoch_schedule,
                );
                if let Some(new_orphan_root) = new_orphan_root
                    && new_orphan_root != self.root
                    && let Some(repair_request) = RepairService::request_repair_if_needed(
                        outstanding_repairs,
                        ShredRepairType::Orphan(new_orphan_root),
                    )
                {
                    repairs.push(repair_request);
                    processed_slots.insert(new_orphan_root);
                    new_best_orphan_requests += 1;
                }
            }
        }

        // If there are fewer than `max_new_orphans`, just grab the next
        // available ones.
        for new_orphan in blockstore.orphans_iterator(self.root + 1).unwrap() {
            if new_best_orphan_requests >= max_new_orphans {
                break;
            }
            if let Some(repair_request) = RepairService::request_repair_if_needed(
                outstanding_repairs,
                ShredRepairType::Orphan(new_orphan),
            ) {
                repairs.push(repair_request);
                processed_slots.insert(new_orphan);
                new_best_orphan_requests += 1;
            }
        }
    }

    /// For all remaining trees (orphan and rooted), generate legacy
    /// unknown-last-index probes prioritized by known data shred count.
    fn get_best_unknown_last_index<'db>(
        &mut self,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        slot_meta_cache: &mut AHashMap<Slot, Option<SlotMetaRepair>>,
        full_slots_cache: &AHashMap<Slot, NextSlots>,
        processed_slots: &mut AHashSet<Slot>,
        max_new_repairs: usize,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> Vec<ShredRepairType> {
        let mut repairs = Vec::default();
        for tree in self.trees.values() {
            if repairs.len() >= max_new_repairs {
                break;
            }
            let new_repairs = get_unknown_last_index(
                tree,
                blockstore,
                pinnable_slice,
                slot_meta_cache,
                full_slots_cache,
                processed_slots,
                max_new_repairs - repairs.len(),
                outstanding_repairs,
            );
            repairs.extend(new_repairs);
        }
        repairs
    }

    /// For all remaining trees (orphan and rooted), generate repairs for subtrees that have last
    /// index info but are missing shreds prioritized by how close to completion they are. These
    /// repairs are also prioritized by age of ancestors, so slots close to completion will first
    /// start by repairing broken ancestors.
    fn get_best_closest_completion<'db>(
        &mut self,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        slot_meta_cache: &mut AHashMap<Slot, Option<SlotMetaRepair>>,
        full_slots_cache: &AHashMap<Slot, NextSlots>,
        processed_slots: &mut AHashSet<Slot>,
        max_new_repairs: usize,
        repair_eligibility: &mut RepairEligibility,
        outstanding_repairs: &mut HashMap<ShredRepairType, u64>,
    ) -> (Vec<ShredRepairType>, /* processed slots */ usize) {
        let mut repairs = Vec::default();
        let mut total_processed_slots = 0;
        for tree in self.trees.values() {
            if repairs.len() >= max_new_repairs {
                break;
            }
            let (new_repairs, new_processed_slots) = get_closest_completion(
                tree,
                blockstore,
                pinnable_slice,
                self.root,
                slot_meta_cache,
                full_slots_cache,
                processed_slots,
                max_new_repairs - repairs.len(),
                repair_eligibility,
                outstanding_repairs,
            );
            repairs.extend(new_repairs);
            total_processed_slots += new_processed_slots;
        }
        (repairs, total_processed_slots)
    }

    /// Attempts to chain the orphan subtree rooted at `orphan_tree_root`
    /// to any earlier subtree with new ancestry information in `blockstore`.
    /// Returns the earliest known ancestor of `heaviest_tree_root`.
    fn update_orphan_ancestors(
        &mut self,
        blockstore: &Blockstore,
        mut orphan_tree_root: Slot,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
    ) -> Option<Slot> {
        // Must only be called on existing orphan trees
        assert!(self.trees.contains_key(&orphan_tree_root));

        // Check blockstore for any new parents of the heaviest orphan tree. Attempt
        // to chain the orphan back to the main fork rooted at `self.root`.
        while orphan_tree_root != self.root {
            let (new_ancestors, parent_tree_root) =
                self.find_ancestor_subtree_of_slot(blockstore, orphan_tree_root);
            {
                let heaviest_tree = self
                    .trees
                    .get_mut(&orphan_tree_root)
                    .expect("Orphan must exist");

                // Skip the leaf of the parent tree that the orphan would merge
                // with later in a call to `merge_trees`
                let num_skip = usize::from(parent_tree_root.is_some());

                for ancestor in new_ancestors.iter().skip(num_skip).rev() {
                    // We temporarily use orphan_tree_root as the tree root and later
                    // rename tree root to either the parent_tree_root or the earliest_ancestor
                    if *ancestor >= self.root {
                        self.slot_to_tree
                            .insert(*ancestor, TreeRoot::Root(orphan_tree_root));
                        heaviest_tree.add_root_parent((*ancestor, Hash::default()));
                    }
                }
            }
            if let Some(parent_tree_root) = parent_tree_root {
                // If another subtree is discovered to be the parent
                // of this subtree, merge the two.
                self.merge_trees(
                    TreeRoot::Root(orphan_tree_root),
                    parent_tree_root,
                    *new_ancestors
                        .front()
                        .expect("Must exist leaf to merge to if `tree_to_merge`.is_some()"),
                    epoch_stakes,
                    epoch_schedule,
                );
                let TreeRoot::Root(parent_tree_root) = parent_tree_root;
                orphan_tree_root = parent_tree_root;
            } else {
                // If there's no other subtree to merge with, then return
                // the current known earliest ancestor of this branch
                if let Some(earliest_ancestor) = new_ancestors.front() {
                    assert!(*earliest_ancestor != self.root); // In this case we would merge above
                    let orphan_tree = self
                        .trees
                        .remove(&orphan_tree_root)
                        .expect("orphan tree must exist");
                    if *earliest_ancestor > self.root {
                        self.rename_tree_root(&orphan_tree, TreeRoot::Root(*earliest_ancestor));
                        assert!(self.trees.insert(*earliest_ancestor, orphan_tree).is_none());
                        orphan_tree_root = *earliest_ancestor;
                    } else {
                        self.remove_tree_slots(&orphan_tree);
                        return None;
                    }
                }
                break;
            }
        }

        // Return the (potentially new in the case of some merges)
        // root of this orphan subtree
        Some(orphan_tree_root)
    }

    fn get_tree(&self, tree_root: TreeRoot) -> Option<&HeaviestSubtreeForkChoice> {
        match tree_root {
            TreeRoot::Root(r) => self.trees.get(&r),
        }
    }

    fn get_tree_mut(&mut self, tree_root: TreeRoot) -> Option<&mut HeaviestSubtreeForkChoice> {
        match tree_root {
            TreeRoot::Root(r) => self.trees.get_mut(&r),
        }
    }

    fn remove_tree(&mut self, tree_root: TreeRoot) -> Option<HeaviestSubtreeForkChoice> {
        match tree_root {
            TreeRoot::Root(r) => self.trees.remove(&r),
        }
    }

    fn get_tree_root(&self, slot: Slot) -> Option<TreeRoot> {
        self.slot_to_tree.get(&slot).copied()
    }

    /// Returns true iff `slot1` and `slot2` are both tracked and belong to the same tree
    pub fn same_tree(&self, slot1: Slot, slot2: Slot) -> bool {
        self.get_tree_root(slot1)
            .and_then(|tree_root| self.get_tree(tree_root))
            .map(|tree| tree.contains_block(&(slot2, Hash::default())))
            .unwrap_or(false)
    }

    /// Assumes that `new_tree_root` does not already exist in `self.trees`
    fn insert_new_tree(&mut self, new_tree_root: Slot) {
        assert!(!self.trees.contains_key(&new_tree_root));

        // Update `self.slot_to_tree`
        self.slot_to_tree
            .insert(new_tree_root, TreeRoot::Root(new_tree_root));
        self.trees.insert(
            new_tree_root,
            HeaviestSubtreeForkChoice::new((new_tree_root, Hash::default())),
        );
    }

    /// Finds any ancestors available from `blockstore` for `slot`.
    /// Ancestor search is stopped when finding one that chains to any
    /// tree in `self.trees` or if the ancestor is < self.root.
    ///
    /// Returns ancestors (including the one that triggered the search stop condition),
    /// and possibly the tree_root that `slot` belongs to
    fn find_ancestor_subtree_of_slot(
        &self,
        blockstore: &Blockstore,
        slot: Slot,
    ) -> (VecDeque<Slot>, Option<TreeRoot>) {
        let ancestors = AncestorIterator::new(slot, blockstore);
        let mut ancestors_to_add = VecDeque::new();
        let mut tree = None;
        // This means `heaviest_tree_root` has not been
        // chained back to slots currently being replayed
        // in BankForks. Try to see if blockstore has sufficient
        // information to link this slot back
        for a in ancestors {
            ancestors_to_add.push_front(a);
            // If an ancestor chains back to another subtree, then return
            tree = self.get_tree_root(a);
            if tree.is_some() || a < self.root {
                break;
            }
        }

        (ancestors_to_add, tree)
    }

    /// Attaches `tree1` rooted at `root1` to `tree2` rooted at `root2`
    /// at the leaf in `tree2` given by `merge_leaf`
    /// Expects `root1` and `root2` to exist in `self.trees`
    fn merge_trees(
        &mut self,
        root1: TreeRoot,
        root2: TreeRoot,
        merge_leaf: Slot,
        epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
        epoch_schedule: &EpochSchedule,
    ) {
        // Update self.slot_to_tree to reflect the merge
        let tree1 = self.remove_tree(root1).expect("tree to merge must exist");
        self.rename_tree_root(&tree1, root2);

        // Merge trees
        let tree2 = self
            .get_tree_mut(root2)
            .expect("tree to be merged into must exist");

        tree2.merge(
            tree1,
            &(merge_leaf, Hash::default()),
            epoch_stakes,
            epoch_schedule,
        );
    }

    // Update all slots in the `tree1` to point to `root2`,
    fn rename_tree_root(&mut self, tree1: &HeaviestSubtreeForkChoice, root2: TreeRoot) {
        let all_slots = tree1.all_slots_stake_voted_subtree();
        for ((slot, _), _) in all_slots {
            *self
                .slot_to_tree
                .get_mut(slot)
                .expect("Nodes in tree must exist in `self.slot_to_tree`") = root2;
        }
    }

    fn remove_tree_slots(&mut self, tree: &HeaviestSubtreeForkChoice) {
        for ((slot, _), _) in tree.all_slots_stake_voted_subtree() {
            self.slot_to_tree.remove(slot);
        }
    }

    // Heavier, smaller slots come first
    fn sort_by_stake_weight_slot(slot_stake_voted: &mut [(Slot, u64)]) {
        slot_stake_voted.sort_by(|(slot, stake_voted), (slot_, stake_voted_)| {
            if stake_voted == stake_voted_ {
                slot.cmp(slot_)
            } else {
                stake_voted.cmp(stake_voted_).reverse()
            }
        });
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        itertools::Itertools,
        solana_hash::Hash,
        solana_ledger::{
            blockstore::{Blockstore, make_chaining_slot_entries},
            get_tmp_ledger_path,
        },
        solana_runtime::{bank::Bank, bank_utils},
        trees::tr,
    };

    #[test]
    fn test_sort_by_stake_weight_slot() {
        let mut slots = vec![(3, 30), (2, 30), (5, 31)];
        RepairWeight::sort_by_stake_weight_slot(&mut slots);
        assert_eq!(slots, vec![(5, 31), (2, 30), (3, 30)]);
    }

    #[test]
    fn test_add_votes_invalid() {
        let (blockstore, bank, mut repair_weight) = setup_orphan_repair_weight();
        let root = 3;
        repair_weight.set_root(root);

        // Try to add a vote for slot < root and a slot that is unrooted
        for old_slot in &[2, 4] {
            assert!(!repair_weight.slot_to_tree.contains_key(old_slot));
            let votes = vec![(*old_slot, vec![Pubkey::default()])];
            repair_weight.add_voters(
                &blockstore,
                votes.into_iter(),
                bank.epoch_stakes_map(),
                bank.epoch_schedule(),
            );
            assert!(!repair_weight.trees.contains_key(old_slot));
            assert!(!repair_weight.slot_to_tree.contains_key(old_slot));
        }
    }

    #[test]
    fn test_add_votes() {
        let blockstore = setup_forks();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(3, stake);
        let votes = vec![(1, vote_pubkeys.clone())];

        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // repair_weight should contain one subtree 0->1
        assert_eq!(repair_weight.trees.len(), 1);
        assert_eq!(
            repair_weight
                .trees
                .get(&0)
                .unwrap()
                .ancestors((1, Hash::default())),
            vec![(0, Hash::default())]
        );
        for i in &[0, 1] {
            assert_eq!(
                *repair_weight.slot_to_tree.get(i).unwrap(),
                TreeRoot::Root(0)
            );
        }

        // Add slots 6 and 4 with the same set of pubkeys,
        // should discover the rest of the tree and the weights,
        // and should only count the latest votes
        let votes = vec![(4, vote_pubkeys.clone()), (6, vote_pubkeys)];
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );
        assert_eq!(repair_weight.trees.len(), 1);
        assert_eq!(
            repair_weight
                .trees
                .get(&0)
                .unwrap()
                .ancestors((4, Hash::default()))
                .into_iter()
                .map(|slot_hash| slot_hash.0)
                .collect::<Vec<_>>(),
            vec![2, 1, 0]
        );
        assert_eq!(
            repair_weight
                .trees
                .get(&0)
                .unwrap()
                .ancestors((6, Hash::default()))
                .into_iter()
                .map(|slot_hash| slot_hash.0)
                .collect::<Vec<_>>(),
            vec![5, 3, 1, 0]
        );
        for slot in 0..=6 {
            assert_eq!(
                *repair_weight.slot_to_tree.get(&slot).unwrap(),
                TreeRoot::Root(0)
            );
            let stake_voted_at = repair_weight
                .trees
                .get(&0)
                .unwrap()
                .stake_voted_at(&(slot, Hash::default()))
                .unwrap();
            if slot == 6 {
                assert_eq!(stake_voted_at, 3 * stake);
            } else {
                assert_eq!(stake_voted_at, 0);
            }
        }

        for slot in &[6, 5, 3, 1, 0] {
            let stake_voted_subtree = repair_weight
                .trees
                .get(&0)
                .unwrap()
                .stake_voted_subtree(&(*slot, Hash::default()))
                .unwrap();
            assert_eq!(stake_voted_subtree, 3 * stake);
        }
        for slot in &[4, 2] {
            let stake_voted_subtree = repair_weight
                .trees
                .get(&0)
                .unwrap()
                .stake_voted_subtree(&(*slot, Hash::default()))
                .unwrap();
            assert_eq!(stake_voted_subtree, 0);
        }
    }

    #[test]
    fn test_add_votes_orphans() {
        let blockstore = setup_orphans();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(3, stake);
        let votes = vec![(1, vote_pubkeys.clone()), (8, vote_pubkeys.clone())];

        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Should contain two trees, one for main fork, one for the orphan
        // branch
        assert_eq!(repair_weight.trees.len(), 2);
        assert_eq!(
            repair_weight
                .trees
                .get(&0)
                .unwrap()
                .ancestors((1, Hash::default())),
            vec![(0, Hash::default())]
        );
        assert!(
            repair_weight
                .trees
                .get(&8)
                .unwrap()
                .ancestors((8, Hash::default()))
                .is_empty()
        );

        let votes = vec![(1, vote_pubkeys.clone()), (10, vote_pubkeys.clone())];
        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Should contain two trees, one for main fork, one for the orphan
        // branch
        assert_eq!(repair_weight.trees.len(), 2);
        assert_eq!(
            repair_weight
                .trees
                .get(&0)
                .unwrap()
                .ancestors((1, Hash::default())),
            vec![(0, Hash::default())]
        );
        assert_eq!(
            repair_weight
                .trees
                .get(&8)
                .unwrap()
                .ancestors((10, Hash::default())),
            vec![(8, Hash::default())]
        );

        // Connect orphan back to main fork
        blockstore.add_tree(tr(6) / (tr(8)), true, true, 2, Hash::default());
        assert_eq!(
            AncestorIterator::new(8, &blockstore).collect::<Vec<_>>(),
            vec![6, 5, 3, 1, 0]
        );
        let votes = vec![(11, vote_pubkeys)];

        // Should not resolve orphans because `update_orphan_ancestors` has
        // not been called, but should add to the orphan branch
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );
        assert_eq!(
            repair_weight
                .trees
                .get(&8)
                .unwrap()
                .ancestors((11, Hash::default()))
                .into_iter()
                .map(|slot_hash| slot_hash.0)
                .collect::<Vec<_>>(),
            vec![10, 8]
        );

        for slot in &[8, 10, 11] {
            assert_eq!(
                *repair_weight.slot_to_tree.get(slot).unwrap(),
                TreeRoot::Root(8)
            );
        }
        for slot in 0..=1 {
            assert_eq!(
                *repair_weight.slot_to_tree.get(&slot).unwrap(),
                TreeRoot::Root(0)
            );
        }

        // Call `update_orphan_ancestors` to resolve orphan
        repair_weight.update_orphan_ancestors(
            &blockstore,
            8,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        for slot in &[8, 10, 11] {
            assert_eq!(
                *repair_weight.slot_to_tree.get(slot).unwrap(),
                TreeRoot::Root(0)
            );
        }
        assert_eq!(repair_weight.trees.len(), 1);
        assert!(repair_weight.trees.contains_key(&0));
    }

    #[test]
    fn test_update_orphan_ancestors() {
        let blockstore = setup_orphans();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(3, stake);
        // Create votes for both orphan branches
        let votes = vec![
            (10, vote_pubkeys[0..1].to_vec()),
            (22, vote_pubkeys[1..3].to_vec()),
        ];

        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        assert_eq!(repair_weight.trees.len(), 3);
        // Roots of the orphan branches should exist
        assert!(repair_weight.trees.contains_key(&0));
        assert!(repair_weight.trees.contains_key(&8));
        assert!(repair_weight.trees.contains_key(&20));

        // Call `update_orphan_ancestors` to resolve orphan
        repair_weight.update_orphan_ancestors(
            &blockstore,
            8,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Nothing has changed because no orphans were
        // resolved
        assert_eq!(repair_weight.trees.len(), 3);
        // Roots of the orphan branches should exist
        assert!(repair_weight.trees.contains_key(&0));
        assert!(repair_weight.trees.contains_key(&8));
        assert!(repair_weight.trees.contains_key(&20));

        // Resolve orphans in blockstore
        blockstore.add_tree(tr(6) / (tr(8)), true, true, 2, Hash::default());
        blockstore.add_tree(tr(11) / (tr(20)), true, true, 2, Hash::default());
        // Call `update_orphan_ancestors` to resolve orphan
        repair_weight.update_orphan_ancestors(
            &blockstore,
            20,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Only the main fork should exist now
        assert_eq!(repair_weight.trees.len(), 1);
        assert!(repair_weight.trees.contains_key(&0));
    }

    #[test]
    fn test_get_best_orphans() {
        let blockstore = setup_orphans();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(2, stake);
        let votes = vec![(8, vec![vote_pubkeys[0]]), (20, vec![vote_pubkeys[1]])];
        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Ask for only 1 orphan. Because the orphans have the same weight,
        // should prioritize smaller orphan first
        let mut repairs = vec![];
        let mut outstanding_repairs = HashMap::new();
        let mut processed_slots: AHashSet<Slot> = vec![repair_weight.root].into_iter().collect();
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            1,
            &mut outstanding_repairs,
        );
        assert_eq!(
            repair_weight
                .trees
                .get(&8)
                .unwrap()
                .stake_voted_subtree(&(8, Hash::default()))
                .unwrap(),
            repair_weight
                .trees
                .get(&20)
                .unwrap()
                .stake_voted_subtree(&(20, Hash::default()))
                .unwrap()
        );
        assert_eq!(repairs.len(), 1);
        assert_eq!(outstanding_repairs.len(), repairs.len());
        assert_eq!(repairs[0].slot(), 8);

        // New vote on same orphan branch, without any new slot chaining
        // information blockstore should not resolve the orphan
        repairs = vec![];
        outstanding_repairs = HashMap::new();
        processed_slots = vec![repair_weight.root].into_iter().collect();
        let votes = vec![(10, vec![vote_pubkeys[0]])];
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            1,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 1);
        assert_eq!(outstanding_repairs.len(), repairs.len());
        assert_eq!(repairs[0].slot(), 8);

        // Ask for 2 orphans, should return all the orphans
        repairs = vec![];
        outstanding_repairs = HashMap::new();
        processed_slots = vec![repair_weight.root].into_iter().collect();
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            2,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 2);
        assert_eq!(outstanding_repairs.len(), repairs.len());
        assert_eq!(repairs[0].slot(), 8);
        assert_eq!(repairs[1].slot(), 20);

        // Ensure redundant repairs are not generated.
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            2,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 2);
        assert_eq!(outstanding_repairs.len(), repairs.len());

        // If one orphan gets heavier, should pick that one
        repairs = vec![];
        outstanding_repairs = HashMap::new();
        processed_slots = vec![repair_weight.root].into_iter().collect();
        let votes = vec![(20, vec![vote_pubkeys[0]])];
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            1,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 1);
        assert_eq!(outstanding_repairs.len(), repairs.len());
        assert_eq!(repairs[0].slot(), 20);

        // Resolve the orphans, should now return no
        // orphans
        repairs = vec![];
        outstanding_repairs = HashMap::new();
        processed_slots = vec![repair_weight.root].into_iter().collect();
        blockstore.add_tree(tr(6) / (tr(8)), true, true, 2, Hash::default());
        blockstore.add_tree(tr(11) / (tr(20)), true, true, 2, Hash::default());
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            1,
            &mut outstanding_repairs,
        );
        assert!(repairs.is_empty());
        assert!(outstanding_repairs.is_empty());
    }

    #[test]
    fn test_get_extra_orphans() {
        let blockstore = setup_orphans();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(2, stake);
        let votes = vec![(8, vec![vote_pubkeys[0]])];
        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Should only be one orphan in the `trees` map
        assert_eq!(repair_weight.trees.len(), 2);
        // Roots of the orphan branches should exist
        assert!(repair_weight.trees.contains_key(&0));
        assert!(repair_weight.trees.contains_key(&8));

        // Ask for 2 orphans. Even though there's only one
        // orphan in the `trees` map, we should search for
        // exactly one more of the remaining two
        let mut repairs = vec![];
        let mut outstanding_repairs = HashMap::new();
        let mut processed_slots: AHashSet<Slot> = vec![repair_weight.root].into_iter().collect();
        blockstore.add_tree(tr(100) / (tr(101)), true, true, 2, Hash::default());
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            2,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 2);
        assert_eq!(outstanding_repairs.len(), repairs.len());
        assert_eq!(repairs[0].slot(), 8);
        assert_eq!(repairs[1].slot(), 20);

        // If we ask for 3 orphans, we should get all of them
        let mut repairs = vec![];
        outstanding_repairs = HashMap::new();
        processed_slots = vec![repair_weight.root].into_iter().collect();
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            3,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 3);
        assert_eq!(outstanding_repairs.len(), repairs.len());
        assert_eq!(repairs[0].slot(), 8);
        assert_eq!(repairs[1].slot(), 20);
        assert_eq!(repairs[2].slot(), 100);
    }

    #[test]
    fn test_set_root() {
        let (_, _, mut repair_weight) = setup_orphan_repair_weight();

        // Set root at 1
        repair_weight.set_root(1);
        check_old_root_purged_verify_new_root(0, 1, &repair_weight);

        // Other later slots in the fork should be updated to map to the
        // the new root
        assert_eq!(
            *repair_weight.slot_to_tree.get(&1).unwrap(),
            TreeRoot::Root(1)
        );
        assert_eq!(
            *repair_weight.slot_to_tree.get(&2).unwrap(),
            TreeRoot::Root(1)
        );

        // Trees tracked should be updated
        assert_eq!(repair_weight.trees.get(&1).unwrap().tree_root().0, 1);

        // Orphan slots should not be changed
        for orphan in &[8, 20] {
            assert_eq!(
                repair_weight.trees.get(orphan).unwrap().tree_root().0,
                *orphan
            );
            assert_eq!(
                *repair_weight.slot_to_tree.get(orphan).unwrap(),
                TreeRoot::Root(*orphan)
            );
        }
    }

    #[test]
    fn test_set_missing_root() {
        let (_, _, mut repair_weight) = setup_orphan_repair_weight();

        // Make sure the slot to root has not been added to the set
        let missing_root_slot = 5;
        assert!(!repair_weight.slot_to_tree.contains_key(&missing_root_slot));

        // Set root at 5
        repair_weight.set_root(missing_root_slot);
        check_old_root_purged_verify_new_root(0, missing_root_slot, &repair_weight);

        // Should purge [0, 5) from tree
        for slot in 0..5 {
            assert!(!repair_weight.slot_to_tree.contains_key(&slot));
        }

        // Orphan slots should not be changed
        for orphan in &[8, 20] {
            assert_eq!(
                repair_weight.trees.get(orphan).unwrap().tree_root().0,
                *orphan
            );
            assert_eq!(
                *repair_weight.slot_to_tree.get(orphan).unwrap(),
                TreeRoot::Root(*orphan)
            );
        }
    }

    #[test]
    fn test_set_root_existing_non_root_tree() {
        let (_, _, mut repair_weight) = setup_orphan_repair_weight();

        // Set root in an existing orphan branch, slot 10
        repair_weight.set_root(10);
        check_old_root_purged_verify_new_root(0, 10, &repair_weight);

        // Should purge old root tree [0, 6]
        for slot in 0..6 {
            assert!(!repair_weight.slot_to_tree.contains_key(&slot));
        }

        // Should purge orphan parent as well
        assert!(!repair_weight.slot_to_tree.contains_key(&8));

        // Other higher orphan branch rooted at slot `20` remains unchanged
        assert_eq!(repair_weight.trees.get(&20).unwrap().tree_root().0, 20);
        assert_eq!(
            *repair_weight.slot_to_tree.get(&20).unwrap(),
            TreeRoot::Root(20)
        );
    }

    #[test]
    fn test_find_ancestor_subtree_of_slot() {
        let (blockstore, _, mut repair_weight) = setup_orphan_repair_weight();

        // Ancestor of slot 4 is slot 2, with an existing subtree rooted at 0
        // because there was a vote for a descendant
        assert_eq!(
            repair_weight.find_ancestor_subtree_of_slot(&blockstore, 4),
            (VecDeque::from([2]), Some(TreeRoot::Root(0)))
        );

        // Ancestors of 5 are [1, 3], with an existing subtree rooted at 0
        // because there was a vote for a descendant
        assert_eq!(
            repair_weight.find_ancestor_subtree_of_slot(&blockstore, 5),
            (VecDeque::from([1, 3]), Some(TreeRoot::Root(0)))
        );

        // Ancestors of slot 23 are [20, 22], with an existing subtree of 20
        // because there was a vote for 20
        assert_eq!(
            repair_weight.find_ancestor_subtree_of_slot(&blockstore, 23),
            (VecDeque::from([20, 22]), Some(TreeRoot::Root(20)))
        );

        // Ancestors of slot 31 are [30], with no existing subtree
        blockstore.add_tree(tr(30) / (tr(31)), true, true, 2, Hash::default());
        assert_eq!(
            repair_weight.find_ancestor_subtree_of_slot(&blockstore, 31),
            (VecDeque::from([30]), None),
        );

        // Set a root at 5
        repair_weight.set_root(5);

        // Ancestor of slot 6 shouldn't include anything from before the root
        // at slot 5
        assert_eq!(
            repair_weight.find_ancestor_subtree_of_slot(&blockstore, 6),
            (VecDeque::from([5]), Some(TreeRoot::Root(5)))
        );

        // Chain orphan 8 back to slot 4 on a different fork
        // Will still include a slot (4) < root, but only one such slot
        blockstore.add_tree(tr(4) / (tr(8)), true, true, 2, Hash::default());
        assert_eq!(
            repair_weight.find_ancestor_subtree_of_slot(&blockstore, 8),
            (VecDeque::from([4]), None),
        );

        // Don't skip, even though 8's chain info is present in blockstore since the merge hasn't
        // happened stop there
        blockstore.add_tree(tr(8) / (tr(40) / tr(41)), true, true, 2, Hash::default());
        assert_eq!(
            repair_weight.find_ancestor_subtree_of_slot(&blockstore, 41),
            (VecDeque::from([8, 40]), Some(TreeRoot::Root(8))),
        )
    }

    #[test]
    fn test_split_off_copy_weight() {
        let (blockstore, _, mut repair_weight) = setup_orphan_repair_weight();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(1, stake);
        repair_weight.add_voters(
            &blockstore,
            vec![(6, vote_pubkeys)].into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Simulate dump from replay
        blockstore.clear_unconfirmed_slot(3);
        repair_weight.split_off(3);
        blockstore.clear_unconfirmed_slot(10);
        repair_weight.split_off(10);

        // Verify orphans
        let mut orphans = repair_weight.trees.keys().copied().collect_vec();
        orphans.sort();
        assert_eq!(vec![0, 3, 8, 10, 20], orphans);

        // Verify weighting
        assert_eq!(
            0,
            repair_weight
                .trees
                .get(&8)
                .unwrap()
                .stake_voted_subtree(&(8, Hash::default()))
                .unwrap()
        );
        assert_eq!(
            stake,
            repair_weight
                .trees
                .get(&3)
                .unwrap()
                .stake_voted_subtree(&(3, Hash::default()))
                .unwrap()
        );
        assert_eq!(
            2 * stake,
            repair_weight
                .trees
                .get(&10)
                .unwrap()
                .stake_voted_subtree(&(10, Hash::default()))
                .unwrap()
        );

        // Get best orphans works as usual
        let mut repairs = vec![];
        let mut outstanding_repairs = HashMap::new();
        let mut processed_slots = vec![repair_weight.root].into_iter().collect();
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            4,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 4);
        assert_eq!(repairs[0].slot(), 10);
        assert_eq!(repairs[1].slot(), 20);
        assert_eq!(repairs[2].slot(), 3);
        assert_eq!(repairs[3].slot(), 8);
        assert_eq!(outstanding_repairs.len(), repairs.len());
    }

    #[test]
    fn test_split_off_multi_dump_repair() {
        let blockstore = setup_forks();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(1, stake);
        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            vec![(6, vote_pubkeys)].into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        // Simulate multiple dumps (whole branch is duplicate) from replay
        blockstore.clear_unconfirmed_slot(3);
        repair_weight.split_off(3);
        blockstore.clear_unconfirmed_slot(5);
        repair_weight.split_off(5);
        blockstore.clear_unconfirmed_slot(6);
        repair_weight.split_off(6);

        // Verify orphans
        let mut orphans = repair_weight.trees.keys().copied().collect_vec();
        orphans.sort();
        assert_eq!(vec![0, 3, 5, 6], orphans);

        // Get best orphans works as usual
        let mut repairs = vec![];
        let mut outstanding_repairs = HashMap::new();
        let mut processed_slots = vec![repair_weight.root].into_iter().collect();
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            4,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 3);
        assert_eq!(repairs[0].slot(), 6);
        assert_eq!(repairs[1].slot(), 3);
        assert_eq!(repairs[2].slot(), 5);
        assert_eq!(outstanding_repairs.len(), repairs.len());

        // Simulate repair on 6 and 5
        for (shreds, _) in make_chaining_slot_entries(&[5, 6], 100, 0) {
            blockstore.insert_shreds(shreds, true).unwrap();
        }

        // Verify orphans properly updated and chained
        let mut repairs = vec![];
        outstanding_repairs = HashMap::new();
        let mut processed_slots = vec![repair_weight.root].into_iter().collect();
        repair_weight.get_best_orphans(
            &blockstore,
            &mut processed_slots,
            &mut repairs,
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
            4,
            &mut outstanding_repairs,
        );
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].slot(), 3);
        assert_eq!(outstanding_repairs.len(), repairs.len());

        let mut orphans = repair_weight.trees.keys().copied().collect_vec();
        orphans.sort();
        assert_eq!(orphans, vec![0, 3]);
    }

    fn setup_orphan_repair_weight() -> (Blockstore, Bank, RepairWeight) {
        let blockstore = setup_orphans();
        let stake = 100;
        let (bank, vote_pubkeys) = bank_utils::setup_bank_and_vote_pubkeys_for_tests(2, stake);

        // Add votes for the main fork and orphan forks
        let votes = vec![
            (4, vote_pubkeys.clone()),
            (8, vote_pubkeys.clone()),
            (10, vote_pubkeys.clone()),
            (20, vote_pubkeys),
        ];

        let mut repair_weight = RepairWeight::new(0);
        repair_weight.add_voters(
            &blockstore,
            votes.into_iter(),
            bank.epoch_stakes_map(),
            bank.epoch_schedule(),
        );

        assert!(repair_weight.slot_to_tree.contains_key(&0));

        // Check orphans are present
        for orphan in &[8, 20] {
            assert_eq!(
                repair_weight.trees.get(orphan).unwrap().tree_root().0,
                *orphan
            );
            assert_eq!(
                *repair_weight.slot_to_tree.get(orphan).unwrap(),
                TreeRoot::Root(*orphan)
            );
        }
        (blockstore, bank, repair_weight)
    }

    fn check_old_root_purged_verify_new_root(
        old_root: Slot,
        new_root: Slot,
        repair_weight: &RepairWeight,
    ) {
        // Check old root is purged
        assert!(!repair_weight.trees.contains_key(&old_root));
        assert!(!repair_weight.slot_to_tree.contains_key(&old_root));

        // Validate new root
        assert_eq!(
            repair_weight.trees.get(&new_root).unwrap().tree_root().0,
            new_root
        );
        assert_eq!(
            *repair_weight.slot_to_tree.get(&new_root).unwrap(),
            TreeRoot::Root(new_root)
        );
        assert_eq!(repair_weight.root, new_root);
    }

    fn setup_orphans() -> Blockstore {
        /*
            Build fork structure:
                 slot 0
                   |
                 slot 1
                 /    \
            slot 2    |
               |    slot 3
            slot 4    |
                    slot 5
                      |
                    slot 6

            Orphans:
               slot 8
                  |
               slot 10
                  |
               slot 11

            Orphans:
              slot 20
                 |
              slot 22
                 |
              slot 23
        */

        let blockstore = setup_forks();
        blockstore.add_tree(tr(8) / (tr(10) / (tr(11))), true, true, 2, Hash::default());
        blockstore.add_tree(tr(20) / (tr(22) / (tr(23))), true, true, 2, Hash::default());
        assert!(blockstore.orphan(8).unwrap().is_some());
        blockstore
    }

    fn setup_forks() -> Blockstore {
        /*
            Build fork structure:
                 slot 0
                   |
                 slot 1
                 /    \
            slot 2    |
               |    slot 3
            slot 4    |
                    slot 5
                      |
                    slot 6
        */
        let forks = tr(0) / (tr(1) / (tr(2) / (tr(4))) / (tr(3) / (tr(5) / (tr(6)))));
        let ledger_path = get_tmp_ledger_path!();
        let blockstore = Blockstore::open(&ledger_path).unwrap();
        blockstore.add_tree(forks, false, true, 2, Hash::default());
        blockstore
    }
}
