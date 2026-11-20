use {
    super::*,
    crate::genesis_utils::create_genesis_config,
    agave_votor_messages::consensus_message::BlockId,
    solana_entry::block_component::{BlockHeaderV1, UpdateParentV1},
    solana_runtime::bank::Bank,
    test_case::test_case,
};

const SLOT: Slot = 100;
const PARENT: Slot = 96;

fn fec(component: BlockComponent, index: u32, last: bool) -> (Vec<Shred>, Vec<Shred>) {
    Shredder::new(SLOT, PARENT, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &component,
            last,
            Hash::new_unique(),
            index,
            index,
            &mut ProcessShredsStats::default(),
        )
        .into_iter()
        .partition(Shred::is_data)
}

fn entries() -> BlockComponent {
    BlockComponent::new_entry_batch(create_ticks(1, 1, Hash::new_unique())).unwrap()
}

fn recovery_context_before_activation() -> ShredRecoveryContext {
    // Test genesis enables all features. Leave this one inactive so Observe and
    // Skip tasks really use the old rules; Enforce tasks carry their own requirement.
    let mut bank = Bank::new_for_tests(&create_genesis_config(2).genesis_config);
    bank.deactivate_feature(&agave_feature_set::stricter_shred_validation::id());
    ShredRecoveryContext::new(Arc::new(bank), 0)
}

fn recover(blockstore: &Blockstore, task: ShredRecoveryTask) -> RecoveredShredBatch {
    let mut context = recovery_context_before_activation();
    blockstore
        .recover_shreds_from_task(task, &mut context)
        .unwrap()
}

fn pending_count(blockstore: &Blockstore) -> usize {
    blockstore
        .insert_shreds_lock
        .lock()
        .unwrap()
        .deferred_shreds
        .len()
}

// A retained result can replace a new task only if it supplies every missing data shred.
// A purge can leave different data missing under the same root. Failed checks also
// lack this guarantee, so those results must not suppress a later recovery attempt.
#[test]
fn test_pending_result_must_cover_task_data() {
    let (mut data, coding) = fec(entries(), DATA_SHREDS_PER_FEC_BLOCK as u32, true);
    let missing = data.pop().unwrap();
    let stored_shred_ids = data.drain(16..).map(|shred| shred.id()).collect();
    let mut task = ShredRecoveryTask {
        erasure_set: missing.erasure_set(),
        just_inserted_shreds: data,
        stored_shred_ids,
        merkle_root: missing.merkle_root().unwrap(),
        validation_mode: FecValidationMode::Enforce,
        recovery_work: Some(Arc::new(())),
    };
    task.just_inserted_shreds.push(coding[0].clone());
    let mut result = RecoveredShredBatch {
        erasure_set: task.erasure_set,
        retransmit_shreds: Vec::new(),
        data_shreds: vec![missing.clone()],
        merkle_root: Some(task.merkle_root),
        fec_set_check: Some(FecSetCheckResult::Passed),
        validation_mode: FecValidationMode::Enforce,
        recovery_work: task.recovery_work.clone(),
    };
    assert!(result.satisfies_task(&task));
    // A same-root result from before a purge does not own the replacement's work.
    // Reusing it would drop the new task and lose track of the pending check.
    result.recovery_work = Some(Arc::new(()));
    assert!(!result.satisfies_task(&task));
    result.recovery_work = task.recovery_work.clone();
    // Before a purge, this shred might have been received rather than recovered.
    // That old result cannot fill the new gap, despite its successful same-root check.
    result.data_shreds.clear();
    assert!(!result.satisfies_task(&task));
    // A held completion shred is already an input; it needs only the verdict.
    task.just_inserted_shreds.push(missing);
    assert!(result.satisfies_task(&task));
    for failed in [FecSetCheckResult::Failed, FecSetCheckResult::RebuildFailed] {
        result.fec_set_check = Some(failed);
        assert!(!result.satisfies_task(&task));
    }
    result.fec_set_check = Some(FecSetCheckResult::Passed);
    result.merkle_root = Some(Hash::new_unique());
    assert!(!result.satisfies_task(&task));
}

// The root can advance past a slot's parent while a check result is in the queue.
// Discard that result without stopping insertion of unrelated shreds.
// Check both the parent in the shred header and the parent selected by UpdateParent.
#[test_case(false, false; "shred_parent")]
#[test_case(true, false; "update_parent")]
#[test_case(true, true; "update_parent_other_verdict_first")]
fn test_expired_deferred_parent_does_not_block_insertion(update_parent: bool, reverse: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (data, root) = if update_parent {
        let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: PARENT,
            parent_block_id: BlockId::new_unique(),
        });
        let (data, _) = fec(BlockComponent::new_block_marker(header), 0, false);
        let inserted = blockstore
            .insert_enforced_shreds_for_tests(data, &mut [])
            .unwrap();
        finish(&blockstore, inserted.recovery_tasks);
        let update = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
            new_parent_slot: 92,
            new_parent_block_id: BlockId::new_unique(),
        });
        let (mut data, _) = fec(BlockComponent::new_block_marker(update), 32, false);
        // Hold the marker itself so its effective parent is not in stored metadata.
        let marker = data.remove(0);
        data.push(marker);
        data.extend(fec(entries(), 64, true).0);
        (data, 94)
    } else {
        (fec(entries(), 0, true).0, 98)
    };
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    let mut results: Vec<_> = inserted
        .recovery_tasks
        .into_iter()
        .map(|task| recover(&blockstore, task))
        .collect();
    assert!(
        results
            .iter()
            .all(RecoveredShredBatch::has_validation_verdict)
    );
    if reverse {
        results.reverse();
    }
    blockstore.set_roots(std::iter::once(&root)).unwrap();
    let unrelated_slot = SLOT + 4;
    let unrelated: Vec<_> = Shredder::new(unrelated_slot, root, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &entries(),
            true,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        )
        .into_iter()
        .filter(Shred::is_data)
        .take(1)
        .collect();
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(unrelated, &mut results)
        .unwrap();
    assert!(inserted.recovery_tasks.is_empty());
    assert!(
        blockstore
            .get_data_shred(unrelated_slot, 0)
            .unwrap()
            .is_some()
    );
    assert!(!blockstore.is_dead(unrelated_slot));
    assert_eq!(pending_count(&blockstore), 0);
    assert!(!results[0].has_validation_verdict());
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
}

fn finish(blockstore: &Blockstore, tasks: Vec<ShredRecoveryTask>) {
    for task in tasks {
        let result = recover(blockstore, task);
        assert!(matches!(
            result.fec_set_check,
            Some(FecSetCheckResult::Passed)
        ));
        blockstore
            .insert_enforced_shreds_for_tests(vec![], &mut [result])
            .unwrap();
    }
}

// A result can arrive after the slot was purged and a different version arrived.
// An old successful check must not release the replacement's held shred.
// An old failed check must not mark the replacement dead.
#[test_case(None, false; "clear_unconfirmed")]
#[test_case(None, true; "failed_verdict")]
#[test_case(Some(PurgeType::Exact), false; "exact_purge")]
#[test_case(Some(PurgeType::CompactionFilter), false; "cleanup_purge")]
fn test_deferred_stale_verdict_cannot_publish_replacement(purge: Option<PurgeType>, invalid: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (data, coding) = fec(entries(), 0, true);
    let mut data: Vec<_> = data.into_iter().chain(coding).collect();
    if invalid {
        crate::shred::merkle::sign_with_invalid_parity_for_tests(
            &Keypair::new(),
            &mut data,
            0..crate::shred::CODING_SHREDS_PER_FEC_BLOCK,
        );
    }
    data.truncate(DATA_SHREDS_PER_FEC_BLOCK);
    let mut inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    let old_result = recover(&blockstore, inserted.recovery_tasks.pop().unwrap());
    assert!(old_result.has_validation_verdict());
    assert_eq!(
        matches!(old_result.fec_set_check, Some(FecSetCheckResult::Passed)),
        !invalid
    );
    match purge {
        None => blockstore.clear_unconfirmed_slot(SLOT),
        Some(kind) => blockstore.purge_slots(SLOT, SLOT, kind).unwrap(),
    }
    // Remove the held shred from the insertion state during the purge, before any new insertion.
    assert_eq!(pending_count(&blockstore), 0);
    let (data, _) = fec(entries(), 0, true);
    let held = data.last().unwrap().clone();
    let replacement = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    blockstore
        .insert_enforced_shreds_for_tests(vec![], &mut [old_result])
        .unwrap();
    assert_eq!(pending_count(&blockstore), 1);
    assert!(
        blockstore
            .get_data_shred(SLOT, u64::from(held.index()))
            .unwrap()
            .is_none()
    );
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(!blockstore.is_dead(SLOT));
    finish(&blockstore, replacement.recovery_tasks);
    assert!(blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert_eq!(
        blockstore
            .get_data_shred(SLOT, u64::from(held.index()))
            .unwrap()
            .unwrap(),
        held.payload().as_ref()
    );
    assert!(pending_count(&blockstore) == 0);
}

// The held last-in-slot shred still limits which later indices can be accepted.
// Keep the held payload available to build the duplicate proof.
#[test_case(false; "later_batch")]
#[test_case(true; "same_batch")]
fn test_deferred_last_index_rejects_later_shreds(same_batch: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (mut data, _) = fec(entries(), 0, true);
    let (mut later, _) = fec(entries(), 32, true);
    let rejected = later.remove(0);
    let rejected_index = u64::from(rejected.index());
    let inserted = if same_batch {
        data.push(rejected);
        blockstore
            .insert_enforced_shreds_for_tests(data, &mut [])
            .unwrap()
    } else {
        let inserted = blockstore
            .insert_enforced_shreds_for_tests(data, &mut [])
            .unwrap();
        blockstore
            .insert_enforced_shreds_for_tests(vec![rejected], &mut [])
            .unwrap();
        inserted
    };
    assert!(
        blockstore
            .get_data_shred(SLOT, rejected_index)
            .unwrap()
            .is_none()
    );
    assert!(blockstore.has_duplicate_shreds_in_slot(SLOT));
    assert!(!blockstore.is_dead(SLOT));
    finish(&blockstore, inserted.recovery_tasks);
    assert_eq!(blockstore.meta(SLOT).unwrap().unwrap().last_index, Some(31));
}

// A held boundary marker must not make stored metadata refer to an absent payload.
// After restart, insertion must accept that payload from repair.
#[test_case(false; "block_header")]
#[test_case(true; "update_parent")]
fn test_deferred_marker_can_be_repaired_after_restart(update_parent: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: PARENT,
        parent_block_id: BlockId::new_unique(),
    });
    let (component, index) = if update_parent {
        let (data, _) = fec(BlockComponent::new_block_marker(header), 0, false);
        let inserted = blockstore
            .insert_enforced_shreds_for_tests(data, &mut [])
            .unwrap();
        finish(&blockstore, inserted.recovery_tasks);
        (
            VersionedBlockMarker::from_update_parent(UpdateParentV1 {
                new_parent_slot: 92,
                new_parent_block_id: BlockId::new_unique(),
            }),
            32,
        )
    } else {
        (header, 0)
    };
    let (mut data, _) = fec(BlockComponent::new_block_marker(component), index, true);
    let marker = data.remove(0);
    data.push(marker.clone());
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    assert_eq!(inserted.recovery_tasks.len(), 1);
    assert!(
        blockstore
            .get_data_shred(SLOT, u64::from(index))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        blockstore.meta(SLOT).unwrap().unwrap().replay_fec_set_index,
        0
    );
    drop(inserted);
    drop(blockstore);
    let blockstore = Blockstore::open(path.path()).unwrap();
    let repaired = blockstore
        .insert_enforced_shreds_for_tests(vec![marker], &mut [])
        .unwrap();
    assert_eq!(repaired.recovery_tasks.len(), 1);
    assert!(!blockstore.is_dead(SLOT));
    finish(&blockstore, repaired.recovery_tasks);
    let meta = blockstore.meta(SLOT).unwrap().unwrap();
    assert!(meta.is_full());
    assert_eq!(
        meta.parent_slot,
        Some(if update_parent { 92 } else { PARENT })
    );
    assert_eq!(meta.replay_fec_set_index, index);
    assert!(pending_count(&blockstore) == 0);
}

// Insertion checks must use a held UpdateParent without changing the stored parent.
// Later shreds still use the original header parent, but a second marker is invalid.
#[test_case(false; "later_batches")]
#[test_case(true; "same_batch")]
fn test_deferred_update_parent_controls_later_admission(same_batch: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: PARENT,
        parent_block_id: BlockId::new_unique(),
    });
    let (data, _) = fec(BlockComponent::new_block_marker(header), 0, false);
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    finish(&blockstore, inserted.recovery_tasks);

    let update = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: 92,
        new_parent_block_id: BlockId::new_unique(),
    });
    let (mut data, _) = fec(BlockComponent::new_block_marker(update), 32, false);
    let marker = data.remove(0);
    data.push(marker);
    if !same_batch {
        blockstore
            .insert_enforced_shreds_for_tests(std::mem::take(&mut data), &mut [])
            .unwrap();
        assert_eq!(pending_count(&blockstore), 1);
        assert!(blockstore.get_data_shred(SLOT, 32).unwrap().is_none());
    }

    let (mut later, _) = fec(entries(), 96, false);
    data.push(later.remove(0));
    if !same_batch {
        blockstore
            .insert_enforced_shreds_for_tests(std::mem::take(&mut data), &mut [])
            .unwrap();
        assert!(!blockstore.is_dead(SLOT));
    }

    // Index 63 ends the first marker's data range. The marker at 64 therefore
    // has a valid boundary; it must fail because the held marker already changed parent.
    let second_update = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: 88,
        new_parent_block_id: BlockId::new_unique(),
    });
    let (mut second, _) = fec(BlockComponent::new_block_marker(second_update), 64, false);
    data.push(second.remove(0));
    blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    assert!(blockstore.get_data_shred(SLOT, 32).unwrap().is_none());
    assert!(blockstore.get_data_shred(SLOT, 96).unwrap().is_some());
    let meta = blockstore.meta(SLOT).unwrap().unwrap();
    assert_eq!(meta.parent_slot, Some(PARENT));
    assert_eq!(meta.replay_fec_set_index, 0);
    assert!(blockstore.get_data_shred(SLOT, 64).unwrap().is_none());
    assert!(blockstore.is_dead(SLOT));
    assert_eq!(pending_count(&blockstore), 1);
}

// Applying several results must keep the admission view valid as held data is stored.
// Hold the header, UpdateParent marker, and slot-ending shred. Their results can
// arrive in different orders without exposing an unchecked marker or rejecting itself.
#[test_case(false; "header_first")]
#[test_case(true; "update_parent_first")]
fn test_publish_held_markers_with_cached_admission(marker_first: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: PARENT,
        parent_block_id: BlockId::new_unique(),
    });
    let update = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: 92,
        new_parent_block_id: BlockId::new_unique(),
    });
    let mut data = Vec::new();
    for (index, marker) in [(0, header), (32, update)] {
        let (mut shreds, _) = fec(BlockComponent::new_block_marker(marker), index, false);
        let held = shreds.remove(0);
        shreds.push(held);
        data.extend(shreds);
    }
    data.extend(fec(entries(), 64, true).0);
    let duplicate = data.last().unwrap().clone();
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    assert_eq!(pending_count(&blockstore), 3);
    let meta = blockstore.meta(SLOT).unwrap().unwrap();
    assert!(!meta.is_full());
    assert_eq!(meta.replay_fec_set_index, 0);
    assert_eq!(meta.parent_slot, Some(PARENT));
    let mut results: Vec<_> = inserted
        .recovery_tasks
        .into_iter()
        .map(|task| recover(&blockstore, task))
        .collect();
    if marker_first {
        results.rotate_left(1);
    }
    // Simulate eviction of the cached marker header. The duplicate warms only
    // admission metadata, so publishing UpdateParent must use its own header.
    blockstore
        .update_parent_shred_parent_cache
        .lock()
        .unwrap()
        .clear();
    blockstore
        .insert_enforced_shreds_for_tests(vec![duplicate], &mut results)
        .unwrap();
    assert_eq!(pending_count(&blockstore), 0);
    let meta = blockstore.meta(SLOT).unwrap().unwrap();
    assert!(meta.is_full());
    assert_eq!(meta.parent_slot, Some(92));
    assert_eq!(meta.replay_fec_set_index, 32);
    assert!(!blockstore.is_dead(SLOT));
}

// Switching blocks must drop held data from the old version before copying the
// new block. An old worker result must not change the replacement afterward.
#[test]
fn test_block_switch_discards_deferred_shred() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (data, _) = fec(entries(), 0, true);
    let mut inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    let old_result = recover(&blockstore, inserted.recovery_tasks.pop().unwrap());
    let alternate = BlockLocation::Alternate {
        block_id: Hash::new_unique(),
    };
    let (data, _) = fec(entries(), 0, true);
    let replacement = data.last().unwrap().clone();
    blockstore
        .do_insert_shreds(
            data.iter()
                .map(|shred| (shred.clone(), true, alternate, None)),
            false,
            ShredRecoveryMode::Disabled,
            &mut blockstore.new_pinnable_slice(),
            &mut blockstore.get_write_batch(),
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();
    assert_eq!(pending_count(&blockstore), 1);
    blockstore
        .switch_block_from_alternate(SLOT, alternate)
        .unwrap();
    assert_eq!(pending_count(&blockstore), 0);
    blockstore
        .insert_enforced_shreds_for_tests(vec![], &mut [old_result])
        .unwrap();
    assert_eq!(
        blockstore
            .get_data_shred(SLOT, u64::from(replacement.index()))
            .unwrap()
            .unwrap(),
        replacement.payload().as_ref()
    );
    assert!(blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(!blockstore.is_dead(SLOT));
}

// On insertion failure, clear held data from this batch and earlier batches.
// Results that arrive later must leave missing data incomplete.
// Repair can supply that data and trigger validation again.
#[test]
fn test_failed_insertion_discards_held_data_for_repair() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (data, _) = fec(entries(), 0, true);
    let missing = data.last().unwrap().clone();
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    let parent = blockstore.meta(PARENT).unwrap().unwrap();
    // Force chaining to fail after the next batch adds its held shred to the map.
    blockstore.meta_cf.put_bytes(PARENT, &[]).unwrap();
    let data: Vec<_> = Shredder::new(SLOT + 4, PARENT, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &entries(),
            true,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        )
        .into_iter()
        .filter(Shred::is_data)
        .collect();
    assert!(
        blockstore
            .insert_enforced_shreds_for_tests(data, &mut [])
            .is_err()
    );
    assert!(blockstore.meta(SLOT + 4).unwrap().is_none());
    assert_eq!(pending_count(&blockstore), 0);
    blockstore.meta_cf.put(PARENT, &parent).unwrap();
    finish(&blockstore, inserted.recovery_tasks);
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(blockstore.get_slot_entries(SLOT, 0).unwrap().is_empty());
    let repaired = blockstore
        .insert_enforced_shreds_for_tests(vec![missing], &mut [])
        .unwrap();
    finish(&blockstore, repaired.recovery_tasks);
    assert!(blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert_eq!(pending_count(&blockstore), 0);
}

// Repeated data and new coding shreds must not repeat a held set's pending check.
// If that work is discarded, repair must still be able to schedule a new check.
#[test]
fn test_deferred_validation_scheduled_once_until_repair() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (data, coding) = fec(entries(), 0, true);
    let repeated: Vec<_> = data.iter().cloned().chain(coding).collect();
    let held = data.last().unwrap().clone();
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    assert_eq!(inserted.recovery_tasks.len(), 1);
    let duplicate = blockstore
        .insert_enforced_shreds_for_tests(repeated, &mut [])
        .unwrap();
    assert!(duplicate.recovery_tasks.is_empty());
    drop(inserted);
    assert_eq!(pending_count(&blockstore), 1);
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
    // Repair sees index 31 as missing and sends it again. The old task was discarded.
    let (_, tasks) = blockstore
        .insert_shreds_with_recovered(
            std::iter::once((held, true, BlockLocation::Original, None)),
            &mut [],
            &crate::shred::filter::new_dummy_retransmit_sender(),
            Some(0),
            &mut blockstore.new_pinnable_slice(),
            &mut blockstore.get_write_batch(),
            &|_| panic!("unexpected duplicate proof"),
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();
    assert_eq!(tasks.len(), 1);
    finish(&blockstore, tasks);
    assert!(blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert_eq!(pending_count(&blockstore), 0);
}

fn prepare_unenforced_tasks(blockstore: &Blockstore, shreds: Vec<Shred>) -> Vec<ShredRecoveryTask> {
    blockstore
        .insert_shreds_with_recovered(
            shreds
                .into_iter()
                .map(|shred| (shred, false, BlockLocation::Original, None)),
            &mut [],
            &crate::shred::filter::new_dummy_retransmit_sender(),
            None,
            &mut blockstore.new_pinnable_slice(),
            &mut blockstore.get_write_batch(),
            &|_| panic!("unexpected duplicate proof"),
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap()
        .1
}

// Short epochs can allow a slot's data to arrive before insertion knows that
// SIMD-0504 applies to it. After a purge, the same data must obey the current rules.
// Reuse an old passed check. An old failure must be checked under the current
// rules before it can mark the slot dead or publish held data.
#[test_case(false; "passed")]
#[test_case(true; "failed")]
fn test_observe_result_uses_current_validation_requirement(invalid: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (data, coding) = fec(entries(), 0, true);
    let mut shreds: Vec<_> = data.into_iter().chain(coding).collect();
    if invalid {
        crate::shred::merkle::sign_with_invalid_parity_for_tests(
            &Keypair::new(),
            &mut shreds,
            0..crate::shred::CODING_SHREDS_PER_FEC_BLOCK,
        );
    }
    shreds.truncate(DATA_SHREDS_PER_FEC_BLOCK);
    let mut old_tasks = prepare_unenforced_tasks(&blockstore, shreds.clone());
    assert_eq!(old_tasks.len(), 1);
    assert!(old_tasks[0].validation_mode == FecValidationMode::Observe);
    let old_result = recover(&blockstore, old_tasks.pop().unwrap());

    blockstore.clear_unconfirmed_slot(SLOT);
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(shreds, &mut [])
        .unwrap();
    assert_eq!(pending_count(&blockstore), 1);
    blockstore
        .insert_enforced_shreds_for_tests(vec![], &mut [old_result])
        .unwrap();
    if invalid {
        assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
        assert!(!blockstore.is_dead(SLOT));
        assert_eq!(pending_count(&blockstore), 1);
        let mut results: Vec<_> = inserted
            .recovery_tasks
            .into_iter()
            .map(|task| recover(&blockstore, task))
            .collect();
        assert!(results.iter().all(|result| matches!(
            result.fec_set_check,
            Some(FecSetCheckResult::Failed | FecSetCheckResult::RebuildFailed)
        )));
        blockstore
            .insert_enforced_shreds_for_tests(Vec::new(), &mut results)
            .unwrap();
    }
    assert!(blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert_eq!(blockstore.is_dead(SLOT), invalid);
    assert_eq!(pending_count(&blockstore), 0);
}

// A recovery-only result has no FEC check. After a same-root purge and activation,
// it must not complete a slot that now requires validation. Repair can supply the
// missing shred and start a required check instead.
#[test]
fn test_unchecked_result_cannot_complete_enforced_slot() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (mut data, mut coding) = fec(entries(), 0, true);
    let missing = data.pop().unwrap();
    let mut initial = data.clone();
    initial.push(coding.pop().unwrap());
    // Cross the recovery threshold, but leave the first task unprocessed.
    drop(prepare_unenforced_tasks(&blockstore, initial));
    let mut tasks = prepare_unenforced_tasks(&blockstore, vec![coding.pop().unwrap()]);
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].validation_mode == FecValidationMode::Skip);
    let old_result = recover(&blockstore, tasks.pop().unwrap());
    assert!(old_result.fec_set_check.is_none());
    assert_eq!(old_result.data_shreds.len(), 1);

    blockstore.clear_unconfirmed_slot(SLOT);
    let inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    assert!(inserted.recovery_tasks.is_empty());
    assert_eq!(pending_count(&blockstore), 0);
    blockstore
        .insert_enforced_shreds_for_tests(vec![], &mut [old_result])
        .unwrap();
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(blockstore.get_slot_entries(SLOT, 0).unwrap().is_empty());
    assert!(
        blockstore
            .get_data_shred(SLOT, u64::from(missing.index()))
            .unwrap()
            .is_none()
    );
    let repaired = blockstore
        .insert_enforced_shreds_for_tests(vec![missing], &mut [])
        .unwrap();
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
    finish(&blockstore, repaired.recovery_tasks);
    assert!(blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(!blockstore.is_dead(SLOT));
}

// SIMD-0504 allows different signatures over the same root. A queued check made
// under the old rules must not kill the slot, and an Enforce task must apply its
// rules even before the recovery worker refreshes its root bank.
#[test]
fn test_recovery_uses_required_feature_policy() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (mut data, _) = fec(entries(), 0, true);
    // Signature verification occurs before this API. As in the recovery filter
    // tests, vary signatures here to test their equality rule independently.
    let merkle_root = data[0].merkle_root().unwrap();
    let signature = Keypair::new().sign_message(merkle_root.as_ref());
    let mut payload = data[0].payload().to_vec();
    payload[..signature.as_ref().len()].copy_from_slice(signature.as_ref());
    data[0] = Shred::new_from_serialized_shred(payload).unwrap();
    let mut old_tasks = prepare_unenforced_tasks(&blockstore, data.clone());
    let old_result = recover(&blockstore, old_tasks.pop().unwrap());
    assert!(matches!(
        old_result.fec_set_check,
        Some(FecSetCheckResult::RebuildFailed)
    ));

    blockstore.clear_unconfirmed_slot(SLOT);
    let mut inserted = blockstore
        .insert_enforced_shreds_for_tests(data, &mut [])
        .unwrap();
    blockstore
        .insert_enforced_shreds_for_tests(Vec::new(), &mut [old_result])
        .unwrap();
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(!blockstore.is_dead(SLOT));
    assert_eq!(pending_count(&blockstore), 1);

    let mut context = recovery_context_before_activation();
    assert!(!context.is_stricter_shred_validation_active(SLOT));
    let result = blockstore
        .recover_shreds_from_task(inserted.recovery_tasks.pop().unwrap(), &mut context)
        .unwrap();
    assert!(matches!(
        result.fec_set_check,
        Some(FecSetCheckResult::Passed)
    ));
    blockstore
        .insert_enforced_shreds_for_tests(Vec::new(), &mut [result])
        .unwrap();
    assert!(blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(!blockstore.is_dead(SLOT));
    assert_eq!(pending_count(&blockstore), 0);
}

// A successful FEC check does not establish agreement with the slot's other FEC sets.
// Reconstructed data with a different header parent must still mark the slot dead.
// Its retained coding inputs must not cause this actual insertion failure to wait
// for another recovery attempt.
#[test]
fn test_recovered_parent_conflict_is_not_deferred() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: PARENT,
        parent_block_id: BlockId::new_unique(),
    });
    let (mut shreds, _) = fec(BlockComponent::new_block_marker(header), 0, false);
    let coding = Shredder::new(SLOT, PARENT - 4, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &entries(),
            true,
            Hash::new_unique(),
            32,
            32,
            &mut ProcessShredsStats::default(),
        )
        .into_iter()
        .filter(Shred::is_code);
    shreds.extend(coding);
    let task = prepare_unenforced_tasks(&blockstore, shreds)
        .into_iter()
        .find(|task| task.erasure_set.fec_set_index() == 32)
        .unwrap();
    let result = recover(&blockstore, task);
    assert!(matches!(
        result.fec_set_check,
        Some(FecSetCheckResult::Passed)
    ));
    assert_eq!(result.data_shreds.len(), DATA_SHREDS_PER_FEC_BLOCK);
    assert!(!blockstore.is_dead(SLOT));
    blockstore
        .insert_shreds_with_recovered(
            std::iter::empty(),
            &mut [result],
            &crate::shred::filter::new_dummy_retransmit_sender(),
            None,
            &mut blockstore.new_pinnable_slice(),
            &mut blockstore.get_write_batch(),
            &|_| panic!("unexpected duplicate proof"),
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
    assert!(blockstore.get_data_shred(SLOT, 32).unwrap().is_none());
    assert!(blockstore.is_dead(SLOT));
}
