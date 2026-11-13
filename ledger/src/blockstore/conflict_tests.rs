use {super::*, crate::genesis_utils::create_genesis_config, test_case::test_case};

const SLOT: Slot = 100;
const PARENT: Slot = 96;

fn fec(index: u32, last: bool, parent: Slot) -> (Vec<Shred>, Vec<Shred>) {
    let entries = BlockComponent::new_entry_batch(create_ticks(1, 1, Hash::new_unique())).unwrap();
    Shredder::new(SLOT, parent, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &entries,
            last,
            Hash::new_unique(),
            index,
            index,
            &mut ProcessShredsStats::default(),
        )
        .into_iter()
        .partition(Shred::is_data)
}

fn insert(
    blockstore: &Blockstore,
    shreds: Vec<Shred>,
    results: &mut [RecoveredShredBatch],
    enforce: bool,
) -> Vec<ShredRecoveryTask> {
    blockstore
        .insert_shreds_with_recovered(
            shreds
                .into_iter()
                .map(|shred| (shred, false, BlockLocation::Original, None)),
            results,
            &crate::shred::filter::new_dummy_retransmit_sender(),
            enforce.then_some(0),
            &mut blockstore.new_pinnable_slice(),
            &mut blockstore.get_write_batch(),
            &|_| {},
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap()
        .1
}

fn recover(blockstore: &Blockstore, task: ShredRecoveryTask, version: u16) -> RecoveredShredBatch {
    // Keep Observe and Skip tasks on the old rules. Enforce tasks carry their
    // requirement even when the worker has not refreshed its bank yet.
    let mut bank = Bank::new_for_tests(&create_genesis_config(2).genesis_config);
    bank.deactivate_feature(&agave_feature_set::stricter_shred_validation::id());
    blockstore
        .recover_shreds_from_task(
            task,
            &mut ShredRecoveryContext::new(Arc::new(bank), version),
        )
        .unwrap()
}

fn reject_conflict(
    blockstore: &Blockstore,
    original: &Shred,
    duplicate_worker: bool,
    enforce: bool,
) {
    let (data, coding) = fec(original.fec_set_index(), true, PARENT);
    let conflict = if original.is_data() {
        &data[0]
    } else {
        &coding[0]
    };
    if duplicate_worker {
        blockstore
            .store_duplicate_slot(SLOT, original.payload().clone(), conflict.payload().clone())
            .unwrap();
        blockstore
            .set_dead_slot_if_duplicate_and_not_full(SLOT)
            .unwrap();
    } else {
        insert(blockstore, vec![conflict.clone()], &mut [], enforce);
    }
    assert!(blockstore.has_duplicate_shreds_in_slot(SLOT));
}

// A task remains outstanding after the worker finishes, until insertion applies
// its result. Both conflict paths must wait. Applying the result must also release
// its work record while the caller still owns the result object.
#[test_case(false, false; "insertion_before_activation")]
#[test_case(false, true; "duplicate_worker_before_activation")]
#[test_case(true, false; "insertion_after_activation")]
#[test_case(true, true; "duplicate_worker_after_activation")]
fn test_conflict_waits_until_recovery_result_is_applied(enforce: bool, duplicate_worker: bool) {
    for last in [false, true] {
        let path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(path.path()).unwrap();
        let (mut data, coding) = fec(0, last, PARENT);
        let original = data[0].clone();
        data.remove(5);
        data.push(coding[0].clone());
        let mut tasks = insert(&blockstore, data, &mut [], enforce);
        assert_eq!(tasks.len(), 1);
        reject_conflict(&blockstore, &original, duplicate_worker, enforce);
        assert!(!blockstore.is_dead(SLOT));
        // New coding data can queue another task before the first result arrives.
        // Actual completion must settle the conflict without waiting for that task.
        let duplicate_tasks = if last {
            let tasks = insert(&blockstore, vec![coding[1].clone()], &mut [], enforce);
            assert_eq!(tasks.len(), 1);
            tasks
        } else {
            Vec::new()
        };

        let mut results = [recover(&blockstore, tasks.pop().unwrap(), 0)];
        blockstore.process_pending_recovery_conflicts().unwrap();
        assert!(!blockstore.is_dead(SLOT));
        assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());

        insert(&blockstore, Vec::new(), &mut results, enforce);
        blockstore.process_pending_recovery_conflicts().unwrap();
        assert_eq!(blockstore.meta(SLOT).unwrap().unwrap().is_full(), last);
        assert_eq!(blockstore.is_dead(SLOT), !last);
        assert!(
            !blockstore
                .insert_shreds_lock
                .lock()
                .unwrap()
                .pending_dead_slots
                .contains(&SLOT)
        );
        drop(duplicate_tasks);
        blockstore.process_pending_recovery_conflicts().unwrap();
        assert_eq!(blockstore.is_dead(SLOT), !last);
        // Keep the result alive through these assertions. Completion depends on
        // application, not on when the caller frees its result buffer.
        drop(results);
    }
}

// Full queues, deduplication, or failed reads can discard work without returning
// a result. The idle check must resolve the conflict without another shred arrival.
#[test_case(false; "discarded_task")]
#[test_case(true; "discarded_result")]
fn test_discarded_recovery_resolves_conflict_without_new_input(recovered: bool) {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (mut data, coding) = fec(0, true, PARENT);
    let original = data[0].clone();
    data.remove(5);
    data.push(coding[0].clone());
    let mut tasks = insert(&blockstore, data, &mut [], false);
    assert_eq!(tasks.len(), 1);
    reject_conflict(&blockstore, &original, false, false);
    assert!(!blockstore.is_dead(SLOT));
    let task = tasks.pop().unwrap();
    if recovered {
        drop(recover(&blockstore, task, 0));
    } else {
        drop(task);
    }
    blockstore.process_pending_recovery_conflicts().unwrap();
    assert!(blockstore.is_dead(SLOT));
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
}

// A purge ends the old slot's work record even if its result is still in a queue.
// Reusing the same payloads and Merkle root must not join the two generations.
#[test]
fn test_same_root_reinsert_does_not_wait_for_pre_purge_work() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (mut data, coding) = fec(0, true, PARENT);
    let original = data[0].clone();
    data.remove(5);
    data.push(coding[0].clone());
    let mut old_tasks = insert(&blockstore, data.clone(), &mut [], true);
    assert_eq!(old_tasks.len(), 1);
    let old_result = recover(&blockstore, old_tasks.pop().unwrap(), 0);
    blockstore.clear_unconfirmed_slot(SLOT);

    let tasks = insert(&blockstore, data, &mut [], true);
    assert_eq!(tasks.len(), 1);
    reject_conflict(&blockstore, &original, true, true);
    assert!(!blockstore.is_dead(SLOT));
    drop(tasks);
    blockstore.process_pending_recovery_conflicts().unwrap();
    assert!(blockstore.is_dead(SLOT));
    // The old result is still owned. Only work for the replacement can delay
    // the replacement's conflict decision.
    drop(old_result);
}

// All data can be present while its final shred is held for validation. Dropping
// duplicate jobs must not turn a rejected conflict into a dead slot in this case.
#[test]
fn test_complete_held_data_survives_discarded_duplicate_jobs() {
    let path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(path.path()).unwrap();
    let (data, _) = fec(0, true, PARENT);
    let original = data[0].clone();
    let held = data.last().unwrap().clone();
    let tasks = insert(&blockstore, data, &mut [], true);
    assert_eq!(tasks.len(), 1);
    let (_, retry) = blockstore
        .insert_shreds_with_recovered(
            std::iter::once((held, true, BlockLocation::Original, None)),
            &mut [],
            &crate::shred::filter::new_dummy_retransmit_sender(),
            Some(0),
            &mut blockstore.new_pinnable_slice(),
            &mut blockstore.get_write_batch(),
            &|_| {},
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();
    assert_eq!(retry.len(), 1);
    reject_conflict(&blockstore, &original, false, true);
    drop(tasks);
    drop(retry);
    blockstore.process_pending_recovery_conflicts().unwrap();
    blockstore
        .set_dead_slot_if_duplicate_and_not_full(SLOT)
        .unwrap();
    assert!(!blockstore.is_dead(SLOT));
    // The conflict guard must not publish held data or bypass validation.
    assert!(!blockstore.meta(SLOT).unwrap().unwrap().is_full());
}
