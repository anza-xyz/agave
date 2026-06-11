use {
    super::*,
    crate::{
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        shred::{
            ShredFlags, max_ticks_per_n_shreds,
            merkle::finish_erasure_batch_for_tests,
            merkle_tree::{SIZE_OF_MERKLE_PROOF_ENTRY, get_proof_size, verify_merkle_proof},
        },
    },
    agave_feature_set::discard_unexpected_data_complete_shreds,
    assert_matches::assert_matches,
    crossbeam_channel::unbounded,
    rand::{rng, seq::SliceRandom},
    solana_account_decoder::parse_token::UiTokenAmount,
    solana_entry::entry::next_entry_mut,
    solana_genesis_utils::{MAX_GENESIS_ARCHIVE_UNPACKED_SIZE, open_genesis_config},
    solana_hash::Hash,
    solana_leader_schedule::{FixedSchedule, LeaderSchedule, SlotLeader},
    solana_message::{compiled_instruction::CompiledInstruction, v0::LoadedAddresses},
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_runtime::bank::{Bank, RewardType},
    solana_sha256_hasher::hash,
    solana_shred_version::version_from_hash,
    solana_signature::Signature,
    solana_storage_proto::convert::generated,
    solana_streamer::evicting_sender::EvictingSender,
    solana_transaction::Transaction,
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::TransactionError,
    solana_transaction_status::{
        InnerInstruction, InnerInstructions, Reward, Rewards, TransactionTokenBalance,
    },
    std::{cmp::Ordering, num::NonZeroUsize, time::Duration},
    test_case::{test_case, test_matrix},
};

// used for tests only
pub(crate) fn make_slot_entries_with_transactions(num_entries: u64) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for x in 0..num_entries {
        let transaction = Transaction::new_with_compiled_instructions(
            &[&Keypair::new()],
            &[solana_pubkey::new_rand()],
            Hash::default(),
            vec![solana_pubkey::new_rand()],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        entries.push(next_entry_mut(&mut Hash::default(), 0, vec![transaction]));
        let mut tick = create_ticks(1, 0, hash(&bincode::serialize(&x).unwrap()));
        entries.append(&mut tick);
    }
    entries
}

fn make_and_insert_slot(blockstore: &Blockstore, slot: Slot, parent_slot: Slot) {
    let (shreds, _) = make_slot_entries(
        slot,
        parent_slot,
        100, // num_entries
    );
    blockstore.insert_shreds(shreds, None, true).unwrap();

    let meta = blockstore.meta(slot).unwrap().unwrap();
    assert_eq!(slot, meta.slot);
    assert!(meta.is_full());
    assert!(meta.next_slots.is_empty());
}

fn create_update_parent_shreds_with_shred_parent(
    slot: Slot,
    shred_parent_slot: Slot,
    parent_slot: Slot,
    parent_block_id: Hash,
    shred_index: u32,
    is_last_in_slot: bool,
) -> Vec<Shred> {
    use solana_entry::block_component::UpdateParentV1;
    let component = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: parent_slot,
        new_parent_block_id: parent_block_id,
    });
    let component = BlockComponent::new_block_marker(component);

    Shredder::new(slot, shred_parent_slot, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &component,
            is_last_in_slot,
            Hash::new_unique(),
            shred_index,
            shred_index,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
        .collect()
}

fn create_block_header_shreds(slot: Slot, parent_slot: Slot, parent_block_id: Hash) -> Vec<Shred> {
    create_block_header_shreds_with_shred_parent(slot, parent_slot, parent_slot, parent_block_id)
}

fn create_block_header_shreds_with_shred_parent(
    slot: Slot,
    shred_parent_slot: Slot,
    parent_slot: Slot,
    parent_block_id: Hash,
) -> Vec<Shred> {
    use solana_entry::block_component::BlockHeaderV1;
    let component = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot,
        parent_block_id,
    });
    let component = BlockComponent::new_block_marker(component);

    Shredder::new(slot, shred_parent_slot, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &component,
            false,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
        .collect()
}

fn verify_next_slots(blockstore: &Blockstore, parent_slot: Slot, expected: &[Slot]) {
    let meta = blockstore.meta(parent_slot).unwrap();
    let actual = meta.as_ref().map(|m| {
        let mut slots = m.next_slots.clone();
        slots.sort_unstable();
        slots
    });

    let mut expected = expected.to_vec();
    expected.sort_unstable();

    match (actual, expected.is_empty()) {
        (Some(actual), _) => assert_eq!(
            actual, expected,
            "Parent slot {parent_slot} next_slots mismatch",
        ),
        (None, false) => panic!("Slot {parent_slot} meta doesn't exist"),
        (None, true) => {} // OK - no meta and no expected children
    }
}

fn create_block_footer_shreds(slot: Slot, parent_slot: Slot, shred_index: u32) -> Vec<Shred> {
    create_block_footer_shreds_with_last(slot, parent_slot, shred_index, true)
}

fn create_block_footer_shreds_with_last(
    slot: Slot,
    parent_slot: Slot,
    shred_index: u32,
    is_last_in_slot: bool,
) -> Vec<Shred> {
    use solana_entry::block_component::BlockFooterV1;
    let footer = BlockFooterV1 {
        bank_hash: Hash::new_unique(),
        block_producer_time_nanos: 0,
        block_user_agent: vec![],
        block_final_cert: None,
        skip_reward_cert: None,
        notar_reward_cert: None,
    };
    let component = VersionedBlockMarker::from_block_footer(footer);
    let component = BlockComponent::new_block_marker(component);

    Shredder::new(slot, parent_slot, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &component,
            is_last_in_slot,
            Hash::new_unique(),
            shred_index,
            shred_index,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
        .collect()
}

fn data_shreds(shreds: Vec<Shred>) -> Vec<Shred> {
    shreds.into_iter().filter(Shred::is_data).collect()
}

#[test]
fn test_create_new_ledger() {
    agave_logger::setup();
    let mint_total = 1_000_000_000_000;
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(mint_total);
    let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
    let blockstore = Blockstore::open(ledger_path.path()).unwrap(); //FINDME

    let ticks = create_ticks(genesis_config.ticks_per_slot, 0, genesis_config.hash());
    let entries = blockstore.get_slot_entries(0, 0).unwrap();

    assert_eq!(ticks, entries);
    assert!(
        Path::new(ledger_path.path())
            .join(BLOCKSTORE_DIRECTORY_ROCKS_LEVEL)
            .exists()
    );

    assert_eq!(
        genesis_config,
        open_genesis_config(ledger_path.path(), MAX_GENESIS_ARCHIVE_UNPACKED_SIZE).unwrap()
    );
    // Remove DEFAULT_GENESIS_FILE to force extraction of DEFAULT_GENESIS_ARCHIVE
    std::fs::remove_file(ledger_path.path().join(DEFAULT_GENESIS_FILE)).unwrap();
    assert_eq!(
        genesis_config,
        open_genesis_config(ledger_path.path(), MAX_GENESIS_ARCHIVE_UNPACKED_SIZE).unwrap()
    );
}

#[test]
fn test_insert_get_bytes() {
    // Create enough entries to ensure there are at least two shreds created
    let num_entries = max_ticks_per_n_shreds(1, None) + 1;
    assert!(num_entries > 1);

    let (mut shreds, _) = make_slot_entries(
        0, // slot
        0, // parent_slot
        num_entries,
    );

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Insert last shred, test we can retrieve it
    let last_shred = shreds.pop().unwrap();
    assert!(last_shred.index() > 0);
    blockstore
        .insert_shreds(vec![last_shred.clone()], None, false)
        .unwrap();

    let serialized_shred = blockstore
        .data_shred_cf
        .get_bytes((0, last_shred.index() as u64))
        .unwrap()
        .unwrap();
    let deserialized_shred = Shred::new_from_serialized_shred(serialized_shred).unwrap();

    assert_eq!(last_shred, deserialized_shred);
}

#[test]
fn test_write_entries() {
    agave_logger::setup();
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let ticks_per_slot = 10;
    let num_slots = 10;
    let mut ticks = vec![];
    //let mut shreds_per_slot = 0 as u64;
    let mut shreds_per_slot = vec![];

    for i in 0..num_slots {
        let mut new_ticks = create_ticks(ticks_per_slot, 0, Hash::default());
        let num_shreds = blockstore
            .write_entries(
                i,
                0,
                0,
                ticks_per_slot,
                Some(i.saturating_sub(1)),
                true,
                &Arc::new(Keypair::new()),
                new_ticks.clone(),
                0,
            )
            .unwrap() as u64;
        shreds_per_slot.push(num_shreds);
        ticks.append(&mut new_ticks);
    }

    for i in 0..num_slots {
        let meta = blockstore.meta(i).unwrap().unwrap();
        let num_shreds = shreds_per_slot[i as usize];
        assert_eq!(meta.consumed, num_shreds);
        assert_eq!(meta.received, num_shreds);
        assert_eq!(meta.last_index, Some(num_shreds - 1));
        if i == num_slots - 1 {
            assert!(meta.next_slots.is_empty());
        } else {
            assert_eq!(meta.next_slots, vec![i + 1]);
        }
        if i == 0 {
            assert_eq!(meta.parent_slot, Some(0));
        } else {
            assert_eq!(meta.parent_slot, Some(i - 1));
        }

        assert_eq!(
            &ticks[(i * ticks_per_slot) as usize..((i + 1) * ticks_per_slot) as usize],
            &blockstore.get_slot_entries(i, 0).unwrap()[..]
        );
    }

    /*
                // Simulate writing to the end of a slot with existing ticks
                blockstore
                    .write_entries(
                        num_slots,
                        ticks_per_slot - 1,
                        ticks_per_slot - 2,
                        ticks_per_slot,
                        &ticks[0..2],
                    )
                    .unwrap();

                let meta = blockstore.meta(num_slots).unwrap().unwrap();
                assert_eq!(meta.consumed, 0);
                // received shred was ticks_per_slot - 2, so received should be ticks_per_slot - 2 + 1
                assert_eq!(meta.received, ticks_per_slot - 1);
                // last shred index ticks_per_slot - 2 because that's the shred that made tick_height == ticks_per_slot
                // for the slot
                assert_eq!(meta.last_index, ticks_per_slot - 2);
                assert_eq!(meta.parent_slot, num_slots - 1);
                assert_eq!(meta.next_slots, vec![num_slots + 1]);
                assert_eq!(
                    &ticks[0..1],
                    &blockstore
                        .get_slot_entries(num_slots, ticks_per_slot - 2)
                        .unwrap()[..]
                );

                // We wrote two entries, the second should spill into slot num_slots + 1
                let meta = blockstore.meta(num_slots + 1).unwrap().unwrap();
                assert_eq!(meta.consumed, 1);
                assert_eq!(meta.received, 1);
                assert_eq!(meta.last_index, u64::MAX);
                assert_eq!(meta.parent_slot, num_slots);
                assert!(meta.next_slots.is_empty());

                assert_eq!(
                    &ticks[1..2],
                    &blockstore.get_slot_entries(num_slots + 1, 0).unwrap()[..]
                );
    */
}

#[test]
fn test_put_get_simple() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Test meta column family
    let meta = SlotMeta::new(0, Some(1));
    blockstore.meta_cf.put(0, &meta).unwrap();
    let result = blockstore
        .meta_cf
        .get(0)
        .unwrap()
        .expect("Expected meta object to exist");

    assert_eq!(result, meta);

    // Test erasure column family
    let erasure = vec![1u8; 16];
    let erasure_key = (0, 0);
    blockstore
        .code_shred_cf
        .put_bytes(erasure_key, &erasure)
        .unwrap();

    let result = blockstore
        .code_shred_cf
        .get_bytes(erasure_key)
        .unwrap()
        .expect("Expected erasure object to exist");

    assert_eq!(result, erasure);

    // Test data column family
    let data = vec![2u8; 16];
    let data_key = (0, 0);
    blockstore.data_shred_cf.put_bytes(data_key, &data).unwrap();

    let result = blockstore
        .data_shred_cf
        .get_bytes(data_key)
        .unwrap()
        .expect("Expected data object to exist");

    assert_eq!(result, data);
}

#[test]
fn test_multi_get() {
    const TEST_PUT_ENTRY_COUNT: usize = 100;
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Test meta column family
    for i in 0..TEST_PUT_ENTRY_COUNT {
        let k = u64::try_from(i).unwrap();
        let meta = SlotMeta::new(k, Some(k + 1));
        blockstore.meta_cf.put(k, &meta).unwrap();
        let result = blockstore
            .meta_cf
            .get(k)
            .unwrap()
            .expect("Expected meta object to exist");
        assert_eq!(result, meta);
    }
    let keys = blockstore
        .meta_cf
        .multi_get_keys(0..TEST_PUT_ENTRY_COUNT as Slot);
    let values = blockstore.meta_cf.multi_get(&keys);
    for (i, value) in values.enumerate().take(TEST_PUT_ENTRY_COUNT) {
        let k = u64::try_from(i).unwrap();
        assert_eq!(
            value.as_ref().unwrap().as_ref().unwrap(),
            &SlotMeta::new(k, Some(k + 1))
        );
    }
}

#[test]
fn test_read_shred_bytes() {
    let slot = 0;
    let (shreds, _) = make_slot_entries(slot, 0, 100);
    let num_shreds = shreds.len() as u64;
    let shred_bufs: Vec<_> = shreds.iter().map(Shred::payload).cloned().collect();

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    blockstore.insert_shreds(shreds, None, false).unwrap();

    let mut buf = [0; 4096];
    let (_, bytes) = blockstore.get_data_shreds(slot, 0, 1, &mut buf).unwrap();
    assert_eq!(buf[..bytes], shred_bufs[0][..bytes]);

    let (last_index, bytes2) = blockstore.get_data_shreds(slot, 0, 2, &mut buf).unwrap();
    assert_eq!(last_index, 1);
    assert!(bytes2 > bytes);
    {
        let shred_data_1 = &buf[..bytes];
        assert_eq!(shred_data_1, &shred_bufs[0][..bytes]);

        let shred_data_2 = &buf[bytes..bytes2];
        assert_eq!(shred_data_2, &shred_bufs[1][..bytes2 - bytes]);
    }

    // buf size part-way into shred[1], should just return shred[0]
    let mut buf = vec![0; bytes + 1];
    let (last_index, bytes3) = blockstore.get_data_shreds(slot, 0, 2, &mut buf).unwrap();
    assert_eq!(last_index, 0);
    assert_eq!(bytes3, bytes);

    let mut buf = vec![0; bytes2 - 1];
    let (last_index, bytes4) = blockstore.get_data_shreds(slot, 0, 2, &mut buf).unwrap();
    assert_eq!(last_index, 0);
    assert_eq!(bytes4, bytes);

    let mut buf = vec![0; bytes * 2];
    let (last_index, bytes6) = blockstore
        .get_data_shreds(slot, num_shreds - 1, num_shreds, &mut buf)
        .unwrap();
    assert_eq!(last_index, num_shreds - 1);

    {
        let shred_data = &buf[..bytes6];
        assert_eq!(shred_data, &shred_bufs[(num_shreds - 1) as usize][..bytes6]);
    }

    // Read out of range
    let (last_index, bytes6) = blockstore
        .get_data_shreds(slot, num_shreds, num_shreds + 2, &mut buf)
        .unwrap();
    assert_eq!(last_index, 0);
    assert_eq!(bytes6, 0);
}

#[test]
fn test_shred_cleanup_check() {
    let slot = 1;
    let (shreds, _) = make_slot_entries(slot, 0, 100);

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    blockstore.insert_shreds(shreds, None, false).unwrap();

    let mut buf = [0; 4096];
    assert!(blockstore.get_data_shreds(slot, 0, 1, &mut buf).is_ok());

    let max_purge_slot = 1;
    blockstore
        .purge_slots(0, max_purge_slot, PurgeType::Exact)
        .unwrap();
    *blockstore.lowest_cleanup_slot.write().unwrap() = max_purge_slot;

    let mut buf = [0; 4096];
    assert!(blockstore.get_data_shreds(slot, 0, 1, &mut buf).is_err());
}

#[test]
fn test_insert_data_shreds_basic() {
    // Create enough entries to ensure there are at least two shreds created
    let num_entries = max_ticks_per_n_shreds(1, None) + 1;
    assert!(num_entries > 1);

    let (mut shreds, entries) = make_slot_entries(
        0, // slot
        0, // parent_slot
        num_entries,
    );
    let num_shreds = shreds.len() as u64;

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Insert last shred, we're missing the other shreds, so no consecutive
    // shreds starting from slot 0, index 0 should exist.
    assert!(shreds.len() > 1);
    let last_shred = shreds.pop().unwrap();
    blockstore
        .insert_shreds(vec![last_shred], None, false)
        .unwrap();
    assert!(blockstore.get_slot_entries(0, 0).unwrap().is_empty());

    let meta = blockstore
        .meta(0)
        .unwrap()
        .expect("Expected new metadata object to be created");
    assert!(meta.consumed == 0 && meta.received == num_shreds);

    // Insert the other shreds, check for consecutive returned entries
    blockstore.insert_shreds(shreds, None, false).unwrap();
    let result = blockstore.get_slot_entries(0, 0).unwrap();

    assert_eq!(result, entries);

    let meta = blockstore
        .meta(0)
        .unwrap()
        .expect("Expected new metadata object to exist");
    assert_eq!(meta.consumed, num_shreds);
    assert_eq!(meta.received, num_shreds);
    assert_eq!(meta.parent_slot, Some(0));
    assert_eq!(meta.last_index, Some(num_shreds - 1));
    assert!(meta.next_slots.is_empty());
    assert!(meta.is_connected());
}

#[test]
fn test_insert_data_shreds_reverse() {
    let num_shreds = 10;
    let num_entries = max_ticks_per_n_shreds(num_shreds, None);
    let (mut shreds, entries) = make_slot_entries(
        0, // slot
        0, // parent_slot
        num_entries,
    );
    let num_shreds = shreds.len() as u64;

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Insert shreds in reverse, check for consecutive returned shreds
    for i in (0..num_shreds).rev() {
        let shred = shreds.pop().unwrap();
        blockstore.insert_shreds(vec![shred], None, false).unwrap();
        let result = blockstore.get_slot_entries(0, 0).unwrap();

        let meta = blockstore
            .meta(0)
            .unwrap()
            .expect("Expected metadata object to exist");
        assert_eq!(meta.last_index, Some(num_shreds - 1));
        if i != 0 {
            assert_eq!(result.len(), 0);
            assert!(meta.consumed == 0 && meta.received == num_shreds);
        } else {
            assert_eq!(meta.parent_slot, Some(0));
            assert_eq!(result, entries);
            assert!(meta.consumed == num_shreds && meta.received == num_shreds);
        }
    }
}

#[test]
fn test_insert_slots() {
    test_insert_data_shreds_slots(false);
    test_insert_data_shreds_slots(true);
}

#[test]
fn test_get_slot_entries1() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let entries = create_ticks(8, 0, Hash::default());
    let shreds = entries_to_test_shreds(&entries[0..4], 1, 0, false, 0);
    blockstore
        .insert_shreds(shreds, None, false)
        .expect("Expected successful write of shreds");

    assert_eq!(
        blockstore.get_slot_entries(1, 0).unwrap()[2..4],
        entries[2..4],
    );
}

#[test]
fn test_get_slot_entries3() {
    // Test inserting/fetching shreds which contain multiple entries per shred
    let ledger_path = get_tmp_ledger_path_auto_delete!();

    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let num_slots = 5_u64;
    let shreds_per_slot = 5_u64;
    let entry_serialized_size =
        wincode::serialized_size(&create_ticks(1, 0, Hash::default())).unwrap();
    let entries_per_slot = (shreds_per_slot * PACKET_DATA_SIZE as u64) / entry_serialized_size;

    // Write entries
    for slot in 0..num_slots {
        let entries = create_ticks(entries_per_slot, 0, Hash::default());
        let shreds = entries_to_test_shreds(&entries, slot, slot.saturating_sub(1), false, 0);
        assert!(shreds.len() as u64 >= shreds_per_slot);
        blockstore
            .insert_shreds(shreds, None, false)
            .expect("Expected successful write of shreds");
        assert_eq!(blockstore.get_slot_entries(slot, 0).unwrap(), entries);
    }
}

#[test]
fn test_insert_data_shreds_consecutive() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    // Create enough entries to ensure there are at least two shreds created
    let min_entries = max_ticks_per_n_shreds(1, None) + 1;
    for i in 0..4 {
        let slot = i;
        let parent_slot = if i == 0 { 0 } else { i - 1 };
        // Write entries
        let num_entries = min_entries * (i + 1);
        let (shreds, original_entries) = make_slot_entries(slot, parent_slot, num_entries);

        let num_shreds = shreds.len() as u64;
        assert!(num_shreds > 1);
        let mut even_shreds = vec![];
        let mut odd_shreds = vec![];

        for (i, shred) in shreds.into_iter().enumerate() {
            if i % 2 == 0 {
                even_shreds.push(shred);
            } else {
                odd_shreds.push(shred);
            }
        }

        blockstore.insert_shreds(odd_shreds, None, false).unwrap();

        assert_eq!(blockstore.get_slot_entries(slot, 0).unwrap(), vec![]);

        let meta = blockstore.meta(slot).unwrap().unwrap();
        if num_shreds.is_multiple_of(2) {
            assert_eq!(meta.received, num_shreds);
        } else {
            trace!("got here");
            assert_eq!(meta.received, num_shreds - 1);
        }
        assert_eq!(meta.consumed, 0);
        if num_shreds.is_multiple_of(2) {
            assert_eq!(meta.last_index, Some(num_shreds - 1));
        } else {
            assert_eq!(meta.last_index, None);
        }

        blockstore.insert_shreds(even_shreds, None, false).unwrap();

        assert_eq!(
            blockstore.get_slot_entries(slot, 0).unwrap(),
            original_entries,
        );

        let meta = blockstore.meta(slot).unwrap().unwrap();
        assert_eq!(meta.received, num_shreds);
        assert_eq!(meta.consumed, num_shreds);
        assert_eq!(meta.parent_slot, Some(parent_slot));
        assert_eq!(meta.last_index, Some(num_shreds - 1));
    }
}

#[test]
fn test_data_set_completed_on_insert() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let BlockstoreSignals { blockstore, .. } =
        Blockstore::open_with_signal(ledger_path.path(), BlockstoreOptions::default()).unwrap();

    // Create enough entries to fill 2 shreds, only the later one is data complete
    let slot = 0;
    let num_entries = max_ticks_per_n_shreds(1, None) + 1;
    let entries = create_ticks(num_entries, slot, Hash::default());
    let shreds = entries_to_test_shreds(&entries, slot, 0, true, 0);
    let num_shreds = shreds.len();
    assert!(num_shreds > 1);
    assert!(
        blockstore
            .insert_shreds(shreds[1..].to_vec(), None, false)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        blockstore
            .insert_shreds(vec![shreds[0].clone()], None, false)
            .unwrap(),
        vec![CompletedDataSetInfo {
            slot,
            indices: 0..num_shreds as u32,
        }]
    );
    // Inserting shreds again doesn't trigger notification
    assert!(
        blockstore
            .insert_shreds(shreds, None, false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn test_new_shreds_signal() {
    // Initialize blockstore
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let BlockstoreSignals {
        blockstore,
        ledger_signal_receiver: recvr,
        ..
    } = Blockstore::open_with_signal(ledger_path.path(), BlockstoreOptions::default()).unwrap();

    let entries_per_slot = 50;
    // Create entries for slot 0
    let (mut shreds, _) = make_slot_entries(
        0, // slot
        0, // parent_slot
        entries_per_slot,
    );
    let shreds_per_slot = shreds.len() as u64;

    // Insert second shred, but we're missing the first shred, so no consecutive
    // shreds starting from slot 0, index 0 should exist.
    blockstore
        .insert_shreds(vec![shreds.remove(1)], None, false)
        .unwrap();
    let timer = Duration::from_secs(1);
    assert!(recvr.recv_timeout(timer).is_err());
    // Insert first shred, now we've made a consecutive block
    blockstore
        .insert_shreds(vec![shreds.remove(0)], None, false)
        .unwrap();
    // Wait to get notified of update, should only be one update
    assert!(recvr.recv_timeout(timer).is_ok());
    assert!(recvr.try_recv().is_err());
    // Insert the rest of the ticks
    blockstore.insert_shreds(shreds, None, false).unwrap();
    // Wait to get notified of update, should only be one update
    assert!(recvr.recv_timeout(timer).is_ok());
    assert!(recvr.try_recv().is_err());

    // Create some other slots, and send batches of ticks for each slot such that each slot
    // is missing the tick at shred index == slot index - 1. Thus, no consecutive blocks
    // will be formed
    let num_slots = shreds_per_slot;
    let mut shreds = vec![];
    let mut missing_shreds = vec![];
    for slot in 1..num_slots + 1 {
        let (mut slot_shreds, _) = make_slot_entries(
            slot,
            slot - 1, // parent_slot
            entries_per_slot,
        );
        let missing_shred = slot_shreds.remove(slot as usize - 1);
        shreds.extend(slot_shreds);
        missing_shreds.push(missing_shred);
    }

    // Should be no updates, since no new chains from block 0 were formed
    blockstore.insert_shreds(shreds, None, false).unwrap();
    assert!(recvr.recv_timeout(timer).is_err());

    // For slots 1..num_slots/2, fill in the holes in one batch insertion,
    // so we should only get one signal
    let missing_shreds2 = missing_shreds
        .drain((num_slots / 2) as usize..)
        .collect_vec();
    blockstore
        .insert_shreds(missing_shreds, None, false)
        .unwrap();
    assert!(recvr.recv_timeout(timer).is_ok());
    assert!(recvr.try_recv().is_err());

    // Fill in the holes for each of the remaining slots,
    // we should get a single update
    blockstore
        .insert_shreds(missing_shreds2, None, false)
        .unwrap();

    assert!(recvr.recv_timeout(timer).is_ok());
    assert!(recvr.try_recv().is_err());
}

#[test]
fn test_completed_shreds_signal() {
    // Initialize blockstore
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let BlockstoreSignals {
        blockstore,
        completed_slots_receiver: recvr,
        ..
    } = Blockstore::open_with_signal(ledger_path.path(), BlockstoreOptions::default()).unwrap();

    let entries_per_slot = 10;

    // Create shreds for slot 0
    let (mut shreds, _) = make_slot_entries(0, 0, entries_per_slot);

    let shred0 = shreds.remove(0);
    // Insert all but the first shred in the slot, should not be considered complete
    blockstore.insert_shreds(shreds, None, false).unwrap();
    assert!(recvr.try_recv().is_err());

    // Insert first shred, slot should now be considered complete
    blockstore.insert_shreds(vec![shred0], None, false).unwrap();
    assert_eq!(recvr.try_recv().unwrap(), vec![0]);
}

#[test]
fn test_completed_shreds_signal_orphans() {
    // Initialize blockstore
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let BlockstoreSignals {
        blockstore,
        completed_slots_receiver: recvr,
        ..
    } = Blockstore::open_with_signal(ledger_path.path(), BlockstoreOptions::default()).unwrap();

    let entries_per_slot = 10;
    let slots = [2, 5, 10];
    let mut all_shreds = make_chaining_slot_entries(&slots[..], entries_per_slot, 0);

    // Get the shreds for slot 10, chaining to slot 5
    let (mut orphan_child, _) = all_shreds.remove(2);

    // Get the shreds for slot 5 chaining to slot 2
    let (mut orphan_shreds, _) = all_shreds.remove(1);

    // Insert all but the first shred in the slot, should not be considered complete
    let orphan_child0 = orphan_child.remove(0);
    blockstore.insert_shreds(orphan_child, None, false).unwrap();
    assert!(recvr.try_recv().is_err());

    // Insert first shred, slot should now be considered complete
    blockstore
        .insert_shreds(vec![orphan_child0], None, false)
        .unwrap();
    assert_eq!(recvr.try_recv().unwrap(), vec![slots[2]]);

    // Insert the shreds for the orphan_slot
    let orphan_shred0 = orphan_shreds.remove(0);
    blockstore
        .insert_shreds(orphan_shreds, None, false)
        .unwrap();
    assert!(recvr.try_recv().is_err());

    // Insert first shred, slot should now be considered complete
    blockstore
        .insert_shreds(vec![orphan_shred0], None, false)
        .unwrap();
    assert_eq!(recvr.try_recv().unwrap(), vec![slots[1]]);
}

#[test]
fn test_completed_shreds_signal_many() {
    // Initialize blockstore
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let BlockstoreSignals {
        blockstore,
        completed_slots_receiver: recvr,
        ..
    } = Blockstore::open_with_signal(ledger_path.path(), BlockstoreOptions::default()).unwrap();

    let entries_per_slot = 10;
    let mut slots = vec![2, 5, 10];
    let mut all_shreds = make_chaining_slot_entries(&slots[..], entries_per_slot, 0);
    let disconnected_slot = 4;

    let (shreds0, _) = all_shreds.remove(0);
    let (shreds1, _) = all_shreds.remove(0);
    let (shreds2, _) = all_shreds.remove(0);
    let (shreds3, _) = make_slot_entries(
        disconnected_slot,
        1, // parent_slot
        entries_per_slot,
    );

    let mut all_shreds: Vec<_> = vec![shreds0, shreds1, shreds2, shreds3]
        .into_iter()
        .flatten()
        .collect();

    all_shreds.shuffle(&mut rng());
    blockstore.insert_shreds(all_shreds, None, false).unwrap();
    let mut result = recvr.try_recv().unwrap();
    result.sort_unstable();
    slots.push(disconnected_slot);
    slots.sort_unstable();
    assert_eq!(result, slots);
}

#[test]
fn test_handle_chaining_basic() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let entries_per_slot = 5;
    let num_slots = 3;

    // Construct the shreds
    let (mut shreds, _) = make_many_slot_entries(0, num_slots, entries_per_slot);
    let shreds_per_slot = shreds.len() / num_slots as usize;

    // 1) Write to the first slot
    let shreds1 = shreds
        .drain(shreds_per_slot..2 * shreds_per_slot)
        .collect_vec();
    blockstore.insert_shreds(shreds1, None, false).unwrap();
    let meta1 = blockstore.meta(1).unwrap().unwrap();
    assert!(meta1.next_slots.is_empty());
    // Slot 1 is not connected because slot 0 hasn't been inserted yet
    assert!(!meta1.is_connected());
    assert_eq!(meta1.parent_slot, Some(0));
    assert_eq!(meta1.last_index, Some(shreds_per_slot as u64 - 1));

    // 2) Write to the second slot
    let shreds2 = shreds
        .drain(shreds_per_slot..2 * shreds_per_slot)
        .collect_vec();
    blockstore.insert_shreds(shreds2, None, false).unwrap();
    let meta2 = blockstore.meta(2).unwrap().unwrap();
    assert!(meta2.next_slots.is_empty());
    // Slot 2 is not connected because slot 0 hasn't been inserted yet
    assert!(!meta2.is_connected());
    assert_eq!(meta2.parent_slot, Some(1));
    assert_eq!(meta2.last_index, Some(shreds_per_slot as u64 - 1));

    // Check the first slot again, it should chain to the second slot,
    // but still isn't connected.
    let meta1 = blockstore.meta(1).unwrap().unwrap();
    assert_eq!(meta1.next_slots, vec![2]);
    assert!(!meta1.is_connected());
    assert_eq!(meta1.parent_slot, Some(0));
    assert_eq!(meta1.last_index, Some(shreds_per_slot as u64 - 1));

    // 3) Write to the zeroth slot, check that every slot
    // is now part of the trunk
    blockstore.insert_shreds(shreds, None, false).unwrap();
    for slot in 0..3 {
        let meta = blockstore.meta(slot).unwrap().unwrap();
        // The last slot will not chain to any other slots
        if slot != 2 {
            assert_eq!(meta.next_slots, vec![slot + 1]);
        }
        if slot == 0 {
            assert_eq!(meta.parent_slot, Some(0));
        } else {
            assert_eq!(meta.parent_slot, Some(slot - 1));
        }
        assert_eq!(meta.last_index, Some(shreds_per_slot as u64 - 1));
        assert!(meta.is_connected());
    }
}

#[test]
fn test_handle_chaining_missing_slots() {
    agave_logger::setup();
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let num_slots = 30;
    let entries_per_slot = 5;
    // Make some shreds and split based on whether the slot is odd or even.
    let (shreds, _) = make_many_slot_entries(0, num_slots, entries_per_slot);
    let shreds_per_slot = shreds.len() as u64 / num_slots;
    let (even_slots, odd_slots): (Vec<_>, Vec<_>) =
        shreds.into_iter().partition(|shred| shred.slot() % 2 == 0);

    // Write the odd slot shreds
    blockstore.insert_shreds(odd_slots, None, false).unwrap();

    for slot in 0..num_slots {
        // The slots that were inserted (the odds) will ...
        // - Know who their parent is (parent encoded in the shreds)
        // - Have empty next_slots since next_slots would be evens
        // The slots that were not inserted (the evens) will ...
        // - Still have a meta since their child linked back to them
        // - Have next_slots link to child because of the above
        // - Have an unknown parent since no shreds to indicate
        let meta = blockstore.meta(slot).unwrap().unwrap();
        if slot % 2 == 0 {
            assert_eq!(meta.next_slots, vec![slot + 1]);
            assert_eq!(meta.parent_slot, None);
        } else {
            assert!(meta.next_slots.is_empty());
            assert_eq!(meta.parent_slot, Some(slot - 1));
        }

        // None of the slot should be connected, but since slot 0 is
        // the special case, it will have parent_connected as true.
        assert!(!meta.is_connected());
        assert!(!meta.is_parent_connected() || slot == 0);
    }

    // Write the even slot shreds that we did not earlier
    blockstore.insert_shreds(even_slots, None, false).unwrap();

    for slot in 0..num_slots {
        let meta = blockstore.meta(slot).unwrap().unwrap();
        // All slots except the last one should have a slot in next_slots
        if slot != num_slots - 1 {
            assert_eq!(meta.next_slots, vec![slot + 1]);
        } else {
            assert!(meta.next_slots.is_empty());
        }
        // All slots should have the link back to their parent
        if slot == 0 {
            assert_eq!(meta.parent_slot, Some(0));
        } else {
            assert_eq!(meta.parent_slot, Some(slot - 1));
        }
        // All inserted slots were full and should be connected
        assert_eq!(meta.last_index, Some(shreds_per_slot - 1));
        assert!(meta.is_full());
        assert!(meta.is_connected());
    }
}

#[test]
#[allow(clippy::cognitive_complexity)]
pub fn test_forward_chaining_is_connected() {
    agave_logger::setup();
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let num_slots = 15;
    // Create enough entries to ensure there are at least two shreds created
    let entries_per_slot = max_ticks_per_n_shreds(1, None) + 1;
    assert!(entries_per_slot > 1);

    let (mut shreds, _) = make_many_slot_entries(0, num_slots, entries_per_slot);
    let shreds_per_slot = shreds.len() / num_slots as usize;
    assert!(shreds_per_slot > 1);

    // Write the shreds such that every 3rd slot has a gap in the beginning
    let mut missing_shreds = vec![];
    for slot in 0..num_slots {
        let mut shreds_for_slot = shreds.drain(..shreds_per_slot).collect_vec();
        if slot % 3 == 0 {
            let shred0 = shreds_for_slot.remove(0);
            missing_shreds.push(shred0);
        }
        blockstore
            .insert_shreds(shreds_for_slot, None, false)
            .unwrap();
    }

    // Check metadata
    for slot in 0..num_slots {
        let meta = blockstore.meta(slot).unwrap().unwrap();
        // The last slot will not chain to any other slots
        if slot != num_slots - 1 {
            assert_eq!(meta.next_slots, vec![slot + 1]);
        } else {
            assert!(meta.next_slots.is_empty());
        }

        // Ensure that each slot has their parent correct
        if slot == 0 {
            assert_eq!(meta.parent_slot, Some(0));
        } else {
            assert_eq!(meta.parent_slot, Some(slot - 1));
        }
        // No slots should be connected yet, not even slot 0
        // as slot 0 is still not full yet
        assert!(!meta.is_connected());

        assert_eq!(meta.last_index, Some(shreds_per_slot as u64 - 1));
    }

    // Iteratively finish every 3rd slot, and check that all slots up to and including
    // slot_index + 3 become part of the trunk
    for slot_index in 0..num_slots {
        if slot_index % 3 == 0 {
            let shred = missing_shreds.remove(0);
            blockstore.insert_shreds(vec![shred], None, false).unwrap();

            for slot in 0..num_slots {
                let meta = blockstore.meta(slot).unwrap().unwrap();

                if slot != num_slots - 1 {
                    assert_eq!(meta.next_slots, vec![slot + 1]);
                } else {
                    assert!(meta.next_slots.is_empty());
                }

                if slot < slot_index + 3 {
                    assert!(meta.is_full());
                    assert!(meta.is_connected());
                } else {
                    assert!(!meta.is_connected());
                }

                assert_eq!(meta.last_index, Some(shreds_per_slot as u64 - 1));
            }
        }
    }
}

#[test]
fn test_scan_and_fix_roots() {
    fn blockstore_roots(blockstore: &Blockstore) -> Vec<Slot> {
        blockstore
            .rooted_slot_iterator(0)
            .unwrap()
            .collect::<Vec<_>>()
    }

    agave_logger::setup();
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let entries_per_slot = max_ticks_per_n_shreds(5, None);
    let start_slot: Slot = 0;
    let num_slots = 18;

    // Produce the following chains and insert shreds into Blockstore
    // 0 -> 2 -> 4 -> 6 -> 8 -> 10 -> 12 -> 14 -> 16 -> 18
    //  \
    //   -> 1 -> 3 -> 5 -> 7 ->  9 -> 11 -> 13 -> 15 -> 17
    let shreds: Vec<_> = (start_slot..=num_slots)
        .flat_map(|slot| {
            let parent_slot = if slot % 2 == 0 {
                slot.saturating_sub(2)
            } else {
                slot.saturating_sub(1)
            };
            let (shreds, _) = make_slot_entries(slot, parent_slot, entries_per_slot);
            shreds.into_iter()
        })
        .collect();
    blockstore.insert_shreds(shreds, None, false).unwrap();

    // Start slot must be a root
    let (start, end) = (Some(16), None);
    assert_matches!(
        blockstore.scan_and_fix_roots(start, end, &AtomicBool::new(false)),
        Err(BlockstoreError::SlotNotRooted)
    );

    // Mark several roots
    let new_roots = vec![6, 12];
    blockstore.set_roots(new_roots.iter()).unwrap();
    assert_eq!(&new_roots, &blockstore_roots(&blockstore));

    // Specify both a start root and end slot
    let (start, end) = (Some(12), Some(8));
    let roots = vec![6, 8, 10, 12];
    blockstore
        .scan_and_fix_roots(start, end, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(&roots, &blockstore_roots(&blockstore));

    // Specify only an end slot
    let (start, end) = (None, Some(4));
    let roots = vec![4, 6, 8, 10, 12];
    blockstore
        .scan_and_fix_roots(start, end, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(&roots, &blockstore_roots(&blockstore));

    // Specify only a start slot
    let (start, end) = (Some(12), None);
    let roots = vec![0, 2, 4, 6, 8, 10, 12];
    blockstore
        .scan_and_fix_roots(start, end, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(&roots, &blockstore_roots(&blockstore));

    // Mark additional root
    let new_roots = [16];
    let roots = vec![0, 2, 4, 6, 8, 10, 12, 16];
    blockstore.set_roots(new_roots.iter()).unwrap();
    assert_eq!(&roots, &blockstore_roots(&blockstore));

    // Leave both start and end unspecified
    let (start, end) = (None, None);
    let roots = vec![0, 2, 4, 6, 8, 10, 12, 14, 16];
    blockstore
        .scan_and_fix_roots(start, end, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(&roots, &blockstore_roots(&blockstore));

    // Subsequent calls should have no effect and return without error
    blockstore
        .scan_and_fix_roots(start, end, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(&roots, &blockstore_roots(&blockstore));
}

#[test]
fn test_set_and_chain_connected_on_root_and_next_slots() {
    agave_logger::setup();
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Create enough entries to ensure 5 shreds result
    let entries_per_slot = max_ticks_per_n_shreds(5, None);

    let mut start_slot = 5;
    // Start a chain from a slot not in blockstore, this is the case when
    // node starts with no blockstore and downloads a snapshot. In this
    // scenario, the slot will be marked connected despite its' parent not
    // being connected (or existing) and not being full.
    blockstore
        .set_and_chain_connected_on_root_and_next_slots(start_slot)
        .unwrap();
    let slot_meta5 = blockstore.meta(start_slot).unwrap().unwrap();
    assert!(!slot_meta5.is_full());
    assert!(slot_meta5.is_parent_connected());
    assert!(slot_meta5.is_connected());

    let num_slots = 5;
    // Insert some new slots and ensure they connect to the root correctly
    start_slot += 1;
    let (shreds, _) = make_many_slot_entries(start_slot, num_slots, entries_per_slot);
    blockstore.insert_shreds(shreds, None, false).unwrap();
    for slot in start_slot..start_slot + num_slots {
        info!("Evaluating slot {slot}");
        let meta = blockstore.meta(slot).unwrap().unwrap();
        assert!(meta.is_parent_connected());
        assert!(meta.is_connected());
    }

    // Chain connected on slots that are already connected, should just noop
    blockstore
        .set_and_chain_connected_on_root_and_next_slots(start_slot)
        .unwrap();
    for slot in start_slot..start_slot + num_slots {
        let meta = blockstore.meta(slot).unwrap().unwrap();
        assert!(meta.is_parent_connected());
        assert!(meta.is_connected());
    }

    // Start another chain that is disconnected from previous chain. But, insert
    // a non-full slot and ensure this slot (and its' children) are not marked
    // as connected.
    start_slot += 2 * num_slots;
    let (shreds, _) = make_many_slot_entries(start_slot, num_slots, entries_per_slot);
    // Insert all shreds except for the shreds with index > 0 from non_full_slot
    let non_full_slot = start_slot + num_slots / 2;
    let (shreds, missing_shreds): (Vec<_>, Vec<_>) = shreds
        .into_iter()
        .partition(|shred| shred.slot() != non_full_slot || shred.index() == 0);
    blockstore.insert_shreds(shreds, None, false).unwrap();
    // Chain method hasn't been called yet, so none of these connected yet
    for slot in start_slot..start_slot + num_slots {
        let meta = blockstore.meta(slot).unwrap().unwrap();
        assert!(!meta.is_parent_connected());
        assert!(!meta.is_connected());
    }
    // Now chain from the new starting point
    blockstore
        .set_and_chain_connected_on_root_and_next_slots(start_slot)
        .unwrap();
    for slot in start_slot..start_slot + num_slots {
        let meta = blockstore.meta(slot).unwrap().unwrap();
        match slot.cmp(&non_full_slot) {
            Ordering::Less => {
                // These are fully connected as expected
                assert!(meta.is_parent_connected());
                assert!(meta.is_connected());
            }
            Ordering::Equal => {
                // Parent will be connected, but this slot not connected itself
                assert!(meta.is_parent_connected());
                assert!(!meta.is_connected());
            }
            Ordering::Greater => {
                // All children are not connected either
                assert!(!meta.is_parent_connected());
                assert!(!meta.is_connected());
            }
        }
    }

    // Insert the missing shreds and ensure all slots connected now
    blockstore
        .insert_shreds(missing_shreds, None, false)
        .unwrap();
    for slot in start_slot..start_slot + num_slots {
        let meta = blockstore.meta(slot).unwrap().unwrap();
        assert!(meta.is_parent_connected());
        assert!(meta.is_connected());
    }
}

#[test]
fn test_slot_range_connected_chain() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let num_slots = 3;
    for slot in 1..=num_slots {
        make_and_insert_slot(&blockstore, slot, slot.saturating_sub(1));
    }

    assert!(blockstore.slot_range_connected(1, 3));
    assert!(!blockstore.slot_range_connected(1, 4)); // slot 4 does not exist
}

#[test]
fn test_slot_range_connected_disconnected() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    make_and_insert_slot(&blockstore, 1, 0);
    make_and_insert_slot(&blockstore, 2, 1);
    make_and_insert_slot(&blockstore, 4, 2);

    assert!(blockstore.slot_range_connected(1, 3)); // Slot 3 does not exist, but we can still replay this range to slot 4
    assert!(blockstore.slot_range_connected(1, 4));
}

#[test]
fn test_slot_range_connected_same_slot() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    assert!(blockstore.slot_range_connected(54, 54));
}

#[test]
fn test_slot_range_connected_starting_slot_not_full() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    make_and_insert_slot(&blockstore, 5, 4);
    make_and_insert_slot(&blockstore, 6, 5);

    assert!(!blockstore.meta(4).unwrap().unwrap().is_full());
    assert!(blockstore.slot_range_connected(4, 6));
}

#[test]
fn test_get_slots_since() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Slot doesn't exist
    assert!(blockstore.get_slots_since(&[0]).unwrap().is_empty());

    let mut meta0 = SlotMeta::new(0, Some(0));
    blockstore.meta_cf.put(0, &meta0).unwrap();

    // Slot exists, chains to nothing
    let expected: HashMap<u64, Vec<u64>> = vec![(0, vec![])].into_iter().collect();
    assert_eq!(blockstore.get_slots_since(&[0]).unwrap(), expected);
    meta0.next_slots = vec![1, 2];
    blockstore.meta_cf.put(0, &meta0).unwrap();

    // Slot exists, chains to some other slots
    let expected: HashMap<u64, Vec<u64>> = vec![(0, vec![1, 2])].into_iter().collect();
    assert_eq!(blockstore.get_slots_since(&[0]).unwrap(), expected);
    assert_eq!(blockstore.get_slots_since(&[0, 1]).unwrap(), expected);

    let mut meta3 = SlotMeta::new(3, Some(1));
    meta3.next_slots = vec![10, 5];
    blockstore.meta_cf.put(3, &meta3).unwrap();
    let expected: HashMap<u64, Vec<u64>> = vec![(0, vec![1, 2]), (3, vec![10, 5])]
        .into_iter()
        .collect();
    assert_eq!(blockstore.get_slots_since(&[0, 1, 3]).unwrap(), expected);
}

#[test]
fn test_orphans() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Create shreds and entries
    let entries_per_slot = 1;
    let (mut shreds, _) = make_many_slot_entries(0, 3, entries_per_slot);
    let shreds_per_slot = shreds.len() / 3;

    // Write slot 2, which chains to slot 1. We're missing slot 0,
    // so slot 1 is the orphan
    let shreds_for_slot = shreds.drain((shreds_per_slot * 2)..).collect_vec();
    blockstore
        .insert_shreds(shreds_for_slot, None, false)
        .unwrap();
    let meta = blockstore
        .meta(1)
        .expect("Expect database get to succeed")
        .unwrap();
    assert!(meta.is_orphan());
    assert_eq!(
        blockstore.orphans_iterator(0).unwrap().collect::<Vec<_>>(),
        vec![1]
    );

    // Write slot 1 which chains to slot 0, so now slot 0 is the
    // orphan, and slot 1 is no longer the orphan.
    let shreds_for_slot = shreds.drain(shreds_per_slot..).collect_vec();
    blockstore
        .insert_shreds(shreds_for_slot, None, false)
        .unwrap();
    let meta = blockstore
        .meta(1)
        .expect("Expect database get to succeed")
        .unwrap();
    assert!(!meta.is_orphan());
    let meta = blockstore
        .meta(0)
        .expect("Expect database get to succeed")
        .unwrap();
    assert!(meta.is_orphan());
    assert_eq!(
        blockstore.orphans_iterator(0).unwrap().collect::<Vec<_>>(),
        vec![0]
    );

    // Write some slot that also chains to existing slots and orphan,
    // nothing should change
    let (shred4, _) = make_slot_entries(4, 0, 1);
    let (shred5, _) = make_slot_entries(5, 1, 1);
    blockstore.insert_shreds(shred4, None, false).unwrap();
    blockstore.insert_shreds(shred5, None, false).unwrap();
    assert_eq!(
        blockstore.orphans_iterator(0).unwrap().collect::<Vec<_>>(),
        vec![0]
    );

    // Write zeroth slot, no more orphans
    blockstore.insert_shreds(shreds, None, false).unwrap();
    for i in 0..3 {
        let meta = blockstore
            .meta(i)
            .expect("Expect database get to succeed")
            .unwrap();
        assert!(!meta.is_orphan());
    }
    // Orphans cf is empty
    assert!(blockstore.orphans_cf.is_empty().unwrap());
}

fn test_insert_data_shreds_slots(should_bulk_write: bool) {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Create shreds and entries
    let num_slots = 20_u64;
    let mut entries = vec![];
    let mut shreds = vec![];
    for slot in 0..num_slots {
        let parent_slot = slot.saturating_sub(1);
        let (slot_shreds, entry) = make_slot_entries(slot, parent_slot, 1);
        shreds.extend(slot_shreds);
        entries.extend(entry);
    }

    let num_shreds = shreds.len();
    // Write shreds to the database
    if should_bulk_write {
        blockstore.insert_shreds(shreds, None, false).unwrap();
    } else {
        for _ in 0..num_shreds {
            let shred = shreds.remove(0);
            blockstore.insert_shreds(vec![shred], None, false).unwrap();
        }
    }

    for i in 0..num_slots - 1 {
        assert_eq!(
            blockstore.get_slot_entries(i, 0).unwrap()[0],
            entries[i as usize]
        );

        let meta = blockstore.meta(i).unwrap().unwrap();
        assert_eq!(meta.received, DATA_SHREDS_PER_FEC_BLOCK as u64);
        assert_eq!(meta.last_index, Some(DATA_SHREDS_PER_FEC_BLOCK as u64 - 1));
        assert_eq!(meta.parent_slot, Some(i.saturating_sub(1)));
        assert_eq!(meta.consumed, DATA_SHREDS_PER_FEC_BLOCK as u64);
    }
}

#[test]
fn test_find_missing_data_indexes() {
    let slot = 0;
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Write entries
    let gap: u64 = 10;
    assert!(gap > 3);
    // Create enough entries to ensure there are at least two shreds created
    let entries = create_ticks(1, 0, Hash::default());
    let mut shreds = entries_to_test_shreds(&entries, slot, 0, true, 0);
    shreds.retain(|s| (s.index() % gap as u32) == 0);
    let num_shreds = 2;
    shreds.truncate(num_shreds);
    blockstore.insert_shreds(shreds, None, false).unwrap();

    // Index of the first shred is 0
    // Index of the second shred is "gap"
    // Thus, the missing indexes should then be [1, gap - 1] for the input index
    // range of [0, gap)
    let expected: Vec<u64> = (1..gap).collect();
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot,
            0,            // start_index
            gap,          // end_index
            gap as usize, // max_missing
        ),
        expected
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot,
            1,                  // start_index
            gap,                // end_index
            (gap - 1) as usize, // max_missing
        ),
        expected,
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot,
            0,                  // start_index
            gap - 1,            // end_index
            (gap - 1) as usize, // max_missing
        ),
        &expected[..expected.len() - 1],
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot,
            gap - 2,      // start_index
            gap,          // end_index
            gap as usize, // max_missing
        ),
        vec![gap - 2, gap - 1],
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot,    // slot
            gap - 2, // start_index
            gap,     // end_index
            1,       // max_missing
        ),
        vec![gap - 2],
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot, // slot
            0,    // start_index
            gap,  // end_index
            1,    // max_missing
        ),
        vec![1],
    );

    // Test with a range that encompasses a shred with index == gap which was
    // already inserted.
    let mut expected: Vec<u64> = (1..gap).collect();
    expected.push(gap + 1);
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot,
            0,                  // start_index
            gap + 2,            // end_index
            (gap + 2) as usize, // max_missing
        ),
        expected,
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot,
            0,                  // start_index
            gap + 2,            // end_index
            (gap - 1) as usize, // max_missing
        ),
        &expected[..expected.len() - 1],
    );

    for i in 0..num_shreds as u64 {
        for j in 0..i {
            let expected: Vec<u64> = (j..i)
                .flat_map(|k| {
                    let begin = k * gap + 1;
                    let end = (k + 1) * gap;
                    begin..end
                })
                .collect();
            assert_eq!(
                blockstore.find_missing_data_indexes(
                    slot,
                    j * gap,                  // start_index
                    i * gap,                  // end_index
                    ((i - j) * gap) as usize, // max_missing
                ),
                expected,
            );
        }
    }
}

#[test]
fn test_find_missing_data_indexes_sanity() {
    let slot = 0;

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Early exit conditions
    let empty: Vec<u64> = vec![];
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot, // slot
            0,    // start_index
            0,    // end_index
            1,    // max_missing
        ),
        empty
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot, // slot
            5,    // start_index
            5,    // end_index
            1,    // max_missing
        ),
        empty
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot, // slot
            4,    // start_index
            3,    // end_index
            1,    // max_missing
        ),
        empty
    );
    assert_eq!(
        blockstore.find_missing_data_indexes(
            slot, // slot
            1,    // start_index
            2,    // end_index
            0,    // max_missing
        ),
        empty
    );

    let entries = create_ticks(100, 0, Hash::default());
    let mut shreds = entries_to_test_shreds(&entries, slot, 0, true, 0);

    const ONE: u64 = 1;
    const OTHER: u64 = 4;
    assert!(shreds.len() > OTHER as usize);

    let shreds = vec![shreds.remove(OTHER as usize), shreds.remove(ONE as usize)];

    // Insert one shred at index = first_index
    blockstore.insert_shreds(shreds, None, false).unwrap();

    const STARTS: u64 = OTHER * 2;
    const END: u64 = OTHER * 3;
    const MAX: usize = 10;
    // The first shred has index = first_index. Thus, for i < first_index,
    // given the input range of [i, first_index], the missing indexes should be
    // [i, first_index - 1]
    for start in 0..STARTS {
        let result = blockstore.find_missing_data_indexes(
            slot,  // slot
            start, // start_index
            END,   // end_index
            MAX,   // max_missing
        );
        let expected: Vec<u64> = (start..END).filter(|i| *i != ONE && *i != OTHER).collect();
        assert_eq!(result, expected);
    }
}

#[test]
fn test_no_missing_shred_indexes() {
    let slot = 0;
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Write entries
    let num_entries = 10;
    let entries = create_ticks(num_entries, 0, Hash::default());
    let shreds = entries_to_test_shreds(&entries, slot, 0, true, 0);
    let num_shreds = shreds.len();

    blockstore.insert_shreds(shreds, None, false).unwrap();

    let empty: Vec<u64> = vec![];
    for i in 0..num_shreds as u64 {
        for j in 0..i {
            assert_eq!(
                blockstore.find_missing_data_indexes(
                    slot,
                    j,                // start_index
                    i,                // end_index
                    (i - j) as usize, // max_missing
                ),
                empty
            );
        }
    }
}

#[test]
fn test_verify_shred_slots() {
    // verify_shred_slots(slot, parent, root)
    assert!(verify_shred_slots(0, 0, 0));
    assert!(verify_shred_slots(2, 1, 0));
    assert!(verify_shred_slots(2, 1, 1));
    assert!(!verify_shred_slots(2, 3, 0));
    assert!(!verify_shred_slots(2, 2, 0));
    assert!(!verify_shred_slots(2, 3, 3));
    assert!(!verify_shred_slots(2, 2, 2));
    assert!(!verify_shred_slots(2, 1, 3));
    assert!(!verify_shred_slots(2, 3, 4));
    assert!(!verify_shred_slots(2, 2, 3));
}

#[test]
fn test_should_insert_data_shred() {
    agave_logger::setup();
    let entries = create_ticks(2000, 1, Hash::new_unique());
    let shredder = Shredder::new(0, 0, 1, 0).unwrap();
    let keypair = Keypair::new();
    let rsc = ReedSolomonCache::default();
    let shreds = shredder
        .entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            true,
            Hash::default(), // merkle_root
            0,
            0,
            &rsc,
            &mut ProcessShredsStats::default(),
        )
        .0;
    assert!(
        shreds.len() > DATA_SHREDS_PER_FEC_BLOCK,
        "we want multiple fec sets",
    );
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let max_root = 0;

    // Insert the first 5 shreds, we don't have a "is_last" shred yet
    blockstore
        .insert_shreds(shreds[0..5].to_vec(), None, false)
        .unwrap();

    let slot_meta = blockstore.meta(0).unwrap().unwrap();

    // make a "blank" FEC set that would normally be used to terminate the block
    let terminator = shredder
        .entries_to_merkle_shreds_for_tests(
            &keypair,
            &[],
            true,
            Hash::default(), // merkle_root
            6,               // next_shred_index,
            6,               // next_code_index
            &rsc,
            &mut ProcessShredsStats::default(),
        )
        .0;

    let terminator_shred = terminator.last().unwrap().clone();
    assert!(terminator_shred.last_in_slot());
    assert!(blockstore.should_insert_data_shred(
        &terminator_shred,
        BlockLocation::Original,
        &slot_meta,
        &HashMap::new(),
        max_root,
        None,
        ShredSource::Repaired,
        &mut Vec::new(),
    ));
    let term_last_idx = terminator.last().unwrap().index() as usize;
    // Trying to insert another "is_last" shred with index < the received index should fail
    // so we insert some shreds that are beyond where terminator block is in index space
    blockstore
        .insert_shreds(
            shreds[term_last_idx + 2..term_last_idx + 3].iter().cloned(),
            None,
            false,
        )
        .unwrap();
    let slot_meta = blockstore.meta(0).unwrap().unwrap();
    assert_eq!(slot_meta.received, term_last_idx as u64 + 3);
    let mut duplicate_shreds = vec![];
    // Now we check if terminator is eligible to be inserted
    assert!(
        !blockstore.should_insert_data_shred(
            &terminator_shred,
            BlockLocation::Original,
            &slot_meta,
            &HashMap::new(),
            max_root,
            None,
            ShredSource::Repaired,
            &mut duplicate_shreds,
        ),
        "Should not insert shred with 'last' flag set and index less than already existing shreds"
    );
    assert!(blockstore.has_duplicate_shreds_in_slot(0));
    assert_eq!(duplicate_shreds.len(), 1);
    assert_matches!(
        duplicate_shreds[0],
        PossibleDuplicateShred::LastIndexConflict(_, _)
    );
    assert_eq!(duplicate_shreds[0].slot(), 0);
    let last_idx = shreds.last().unwrap().index();
    // Insert all pending shreds
    blockstore.insert_shreds(shreds, None, false).unwrap();
    let slot_meta = blockstore.meta(0).unwrap().unwrap();

    let past_tail_shreds = shredder
        .entries_to_merkle_shreds_for_tests(
            &Keypair::new(),
            &entries,
            true,
            Hash::default(), // merkle_root
            last_idx,        // next_shred_index,
            last_idx,        // next_code_index
            &rsc,
            &mut ProcessShredsStats::default(),
        )
        .0;

    // Trying to insert a shred with index > the "is_last" shred should fail
    duplicate_shreds.clear();
    blockstore.duplicate_slots_cf.delete(0).unwrap();
    assert!(!blockstore.has_duplicate_shreds_in_slot(0));
    assert!(
        !blockstore.should_insert_data_shred(
            &past_tail_shreds[5], // 5 is not magic, could be any shred from this set
            BlockLocation::Original,
            &slot_meta,
            &HashMap::new(),
            max_root,
            None,
            ShredSource::Repaired,
            &mut duplicate_shreds,
        ),
        "Shreds past end of block should fail to insert"
    );

    assert_eq!(duplicate_shreds.len(), 1);
    assert_matches!(
        duplicate_shreds[0],
        PossibleDuplicateShred::LastIndexConflict(_, _)
    );
    assert_eq!(duplicate_shreds[0].slot(), 0);
    assert!(blockstore.has_duplicate_shreds_in_slot(0));
}

#[test]
fn test_is_data_shred_present() {
    let (shreds, _) = make_slot_entries(0, 0, 200);
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let index_cf = &blockstore.index_cf;

    blockstore
        .insert_shreds(shreds[0..5].to_vec(), None, false)
        .unwrap();
    // Insert a shred less than `slot_meta.consumed`, check that
    // it already exists
    let slot_meta = blockstore.meta(0).unwrap().unwrap();
    let index = index_cf.get(0).unwrap().unwrap();
    assert_eq!(slot_meta.consumed, 5);
    assert!(Blockstore::is_data_shred_present(
        &shreds[1],
        &slot_meta,
        index.data(),
    ));

    // Insert a shred, check that it already exists
    blockstore
        .insert_shreds(shreds[6..7].to_vec(), None, false)
        .unwrap();
    let slot_meta = blockstore.meta(0).unwrap().unwrap();
    let index = index_cf.get(0).unwrap().unwrap();
    assert!(Blockstore::is_data_shred_present(
        &shreds[6],
        &slot_meta,
        index.data()
    ),);
}

#[test]
fn test_merkle_root_metas_coding() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let index = 0;
    let (_, coding_shreds, _) = setup_erasure_shreds(slot, parent_slot, 10);
    let coding_shred = coding_shreds[index as usize].clone();

    let mut shred_insertion_tracker =
        ShredInsertionTracker::new(coding_shreds.len(), blockstore.get_write_batch().unwrap());
    blockstore
        .check_insert_coding_shred(
            Cow::Borrowed(&coding_shred),
            &mut shred_insertion_tracker,
            false,
            ShredSource::Turbine,
        )
        .unwrap();
    let ShredInsertionTracker {
        merkle_root_metas,
        write_batch,
        ..
    } = shred_insertion_tracker;

    assert_eq!(merkle_root_metas.len(), 1);
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .merkle_root(),
        coding_shred.merkle_root().ok(),
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_index(),
        index
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_type(),
        ShredType::Code,
    );

    for ((location, erasure_set), working_merkle_root_meta) in merkle_root_metas {
        assert_eq!(location, BlockLocation::Original);
        blockstore
            .merkle_root_meta_cf
            .put(erasure_set.store_key(), working_merkle_root_meta.as_ref())
            .unwrap();
    }
    blockstore.write_batch(write_batch).unwrap();

    // Add a shred with different merkle root and index
    let (_, coding_shreds, _) = setup_erasure_shreds(slot, parent_slot, 10);
    let new_coding_shred = coding_shreds[(index + 1) as usize].clone();

    let mut shred_insertion_tracker =
        ShredInsertionTracker::new(coding_shreds.len(), blockstore.get_write_batch().unwrap());

    assert!(matches!(
        blockstore.check_insert_coding_shred(
            Cow::Owned(new_coding_shred),
            &mut shred_insertion_tracker,
            false,
            ShredSource::Turbine,
        ),
        Err(InsertCodingShredError::InvalidShred)
    ));

    let ShredInsertionTracker {
        ref merkle_root_metas,
        ref duplicate_shreds,
        ..
    } = shred_insertion_tracker;

    // No insert, notify duplicate
    assert_eq!(duplicate_shreds.len(), 1);
    match &duplicate_shreds[0] {
        PossibleDuplicateShred::MerkleRootConflict(shred, _) if shred.slot() == slot => (),
        _ => panic!("No merkle root conflict"),
    }

    // Verify that we still have the merkle root meta from the original shred
    assert_eq!(merkle_root_metas.len(), 1);
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .merkle_root(),
        coding_shred.merkle_root().ok()
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_index(),
        index
    );

    // Blockstore should also have the merkle root meta of the original shred
    assert_eq!(
        blockstore
            .merkle_root_meta(coding_shred.erasure_set())
            .unwrap()
            .unwrap()
            .merkle_root(),
        coding_shred.merkle_root().ok()
    );
    assert_eq!(
        blockstore
            .merkle_root_meta(coding_shred.erasure_set())
            .unwrap()
            .unwrap()
            .first_received_shred_index(),
        index
    );

    // Add a shred from different fec set
    let new_index = index + 31;
    let (_, coding_shreds, _) = setup_erasure_shreds_with_index(slot, parent_slot, 10, new_index);
    let new_coding_shred = coding_shreds[0].clone();

    blockstore
        .check_insert_coding_shred(
            Cow::Borrowed(&new_coding_shred),
            &mut shred_insertion_tracker,
            false,
            ShredSource::Turbine,
        )
        .unwrap();

    let ShredInsertionTracker {
        ref merkle_root_metas,
        ..
    } = shred_insertion_tracker;

    // Verify that we still have the merkle root meta for the original shred
    // and the new shred
    assert_eq!(merkle_root_metas.len(), 2);
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .merkle_root(),
        coding_shred.merkle_root().ok()
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_index(),
        index
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, new_coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .merkle_root(),
        new_coding_shred.merkle_root().ok()
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, new_coding_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_index(),
        new_index
    );
}

#[test]
fn test_merkle_root_metas_data() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let index = 11;
    let fec_set_index = 11;
    let (data_shreds, _, _) = setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let data_shred = data_shreds[0].clone();

    let mut shred_insertion_tracker =
        ShredInsertionTracker::new(data_shreds.len(), blockstore.get_write_batch().unwrap());
    blockstore
        .check_insert_data_shred(
            Cow::Borrowed(&data_shred),
            BlockLocation::Original,
            &mut shred_insertion_tracker,
            false,
            None,
            ShredSource::Turbine,
        )
        .unwrap();
    let ShredInsertionTracker {
        merkle_root_metas,
        write_batch,
        ..
    } = shred_insertion_tracker;
    assert_eq!(merkle_root_metas.len(), 1);
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, data_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .merkle_root(),
        data_shred.merkle_root().ok()
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, data_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_index(),
        index
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, data_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_type(),
        ShredType::Data,
    );

    for ((location, erasure_set), working_merkle_root_meta) in merkle_root_metas {
        assert_eq!(location, BlockLocation::Original);
        blockstore
            .merkle_root_meta_cf
            .put(erasure_set.store_key(), working_merkle_root_meta.as_ref())
            .unwrap();
    }
    blockstore.write_batch(write_batch).unwrap();

    // Add a shred with different merkle root and index
    let (data_shreds, _, _) = setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let new_data_shred = data_shreds[1].clone();

    let mut shred_insertion_tracker =
        ShredInsertionTracker::new(data_shreds.len(), blockstore.get_write_batch().unwrap());

    let insert_result = blockstore.check_insert_data_shred(
        Cow::Owned(new_data_shred),
        BlockLocation::Original,
        &mut shred_insertion_tracker,
        false,
        None,
        ShredSource::Turbine,
    );
    assert_matches!(
        insert_result.unwrap_err(),
        InsertDataShredError::InvalidShred
    );
    blockstore
        .dead_slots_cf
        .put_in_batch(&mut shred_insertion_tracker.write_batch, slot, &true)
        .unwrap();
    let ShredInsertionTracker {
        merkle_root_metas,
        duplicate_shreds,
        write_batch,
        ..
    } = shred_insertion_tracker;

    // No insert, notify duplicate, and block is dead
    assert_eq!(duplicate_shreds.len(), 1);
    assert_matches!(
        duplicate_shreds[0],
        PossibleDuplicateShred::MerkleRootConflict(_, _)
    );

    // Verify that we still have the merkle root meta from the original shred
    assert_eq!(merkle_root_metas.len(), 1);
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, data_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .merkle_root(),
        data_shred.merkle_root().ok()
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, data_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_index(),
        index
    );

    // Block is now dead
    blockstore.db.write(write_batch).unwrap();
    assert!(blockstore.is_dead(slot));
    blockstore.remove_dead_slot(slot).unwrap();

    // Blockstore should also have the merkle root meta of the original shred
    assert_eq!(
        blockstore
            .merkle_root_meta(data_shred.erasure_set())
            .unwrap()
            .unwrap()
            .merkle_root(),
        data_shred.merkle_root().ok()
    );
    assert_eq!(
        blockstore
            .merkle_root_meta(data_shred.erasure_set())
            .unwrap()
            .unwrap()
            .first_received_shred_index(),
        index
    );

    let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 0).unwrap();
    let keypair = Keypair::new();
    let reed_solomon_cache = ReedSolomonCache::default();
    let new_index = fec_set_index + 31;
    // Add a shred from different fec set
    let new_data_shred = shredder
        .make_shreds_from_data_slice(
            &keypair,
            &[3, 3, 3],
            false,
            Hash::default(),
            new_index,
            new_index,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        )
        .unwrap()
        .next()
        .unwrap();

    let mut shred_insertion_tracker =
        ShredInsertionTracker::new(data_shreds.len(), blockstore.db.batch().unwrap());
    blockstore
        .check_insert_data_shred(
            Cow::Borrowed(&new_data_shred),
            BlockLocation::Original,
            &mut shred_insertion_tracker,
            false,
            None,
            ShredSource::Turbine,
        )
        .unwrap();
    let ShredInsertionTracker {
        merkle_root_metas,
        write_batch,
        ..
    } = shred_insertion_tracker;
    blockstore.db.write(write_batch).unwrap();

    // Verify that we still have the merkle root meta for the original shred
    // and the new shred
    assert_eq!(
        blockstore
            .merkle_root_meta(data_shred.erasure_set())
            .unwrap()
            .as_ref()
            .unwrap()
            .merkle_root(),
        data_shred.merkle_root().ok()
    );
    assert_eq!(
        blockstore
            .merkle_root_meta(data_shred.erasure_set())
            .unwrap()
            .as_ref()
            .unwrap()
            .first_received_shred_index(),
        index
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, new_data_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .merkle_root(),
        new_data_shred.merkle_root().ok()
    );
    assert_eq!(
        merkle_root_metas
            .get(&(BlockLocation::Original, new_data_shred.erasure_set()))
            .unwrap()
            .as_ref()
            .first_received_shred_index(),
        new_index
    );
}

#[test]
fn test_check_insert_coding_shred() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 1;
    let (_data_shreds, code_shreds, _) =
        setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot(
            slot,
            0,
            10,
            0,
            Hash::default(),
            true,
        );
    let coding_shred = code_shreds[0].clone();

    let mut shred_insertion_tracker =
        ShredInsertionTracker::new(1, blockstore.get_write_batch().unwrap());
    blockstore
        .check_insert_coding_shred(
            Cow::Borrowed(&coding_shred),
            &mut shred_insertion_tracker,
            false,
            ShredSource::Turbine,
        )
        .unwrap();

    // insert again fails on dupe
    assert!(matches!(
        blockstore.check_insert_coding_shred(
            Cow::Borrowed(&coding_shred),
            &mut shred_insertion_tracker,
            false,
            ShredSource::Turbine,
        ),
        Err(InsertCodingShredError::Exists)
    ));
    assert_eq!(
        shred_insertion_tracker.duplicate_shreds,
        vec![PossibleDuplicateShred::Exists(coding_shred)]
    );
}

#[test]
fn test_should_insert_coding_shred() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let max_root = 0;

    let slot = 1;
    let (_data_shreds, code_shreds, _) =
        setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot(
            slot,
            0,
            10,
            0,
            Hash::default(),
            true,
        );
    let coding_shred = code_shreds[0].clone();

    assert!(
        Blockstore::should_insert_coding_shred(&coding_shred, max_root),
        "Insertion of a good coding shred should be allowed"
    );

    blockstore
        .insert_shreds(vec![coding_shred.clone()], None, false)
        .expect("Insertion should succeed");

    assert!(
        Blockstore::should_insert_coding_shred(&coding_shred, max_root),
        "Inserting the same shred again should be allowed since this doesn't check for duplicate \
         index"
    );

    assert!(
        Blockstore::should_insert_coding_shred(&code_shreds[1], max_root),
        "Inserting next shred should be allowed"
    );

    assert!(
        !Blockstore::should_insert_coding_shred(&coding_shred, coding_shred.slot()),
        "Trying to insert shred into slot <= last root should not be allowed"
    );
}

#[test]
fn test_insert_multiple_is_last() {
    agave_logger::setup();
    let (shreds, _) = make_slot_entries(0, 0, 18);
    let num_shreds = shreds.len() as u64;
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    blockstore.insert_shreds(shreds, None, false).unwrap();
    let slot_meta = blockstore.meta(0).unwrap().unwrap();

    assert_eq!(slot_meta.consumed, num_shreds);
    assert_eq!(slot_meta.received, num_shreds);
    assert_eq!(slot_meta.last_index, Some(num_shreds - 1));
    assert!(slot_meta.is_full());

    let (shreds, _) = make_slot_entries(0, 0, 600);
    assert!(shreds.len() > num_shreds as usize);
    blockstore.insert_shreds(shreds, None, false).unwrap();
    let slot_meta = blockstore.meta(0).unwrap().unwrap();

    assert_eq!(slot_meta.consumed, num_shreds);
    assert_eq!(slot_meta.received, num_shreds);
    assert_eq!(slot_meta.last_index, Some(num_shreds - 1));
    assert!(slot_meta.is_full());

    assert!(blockstore.has_duplicate_shreds_in_slot(0));
}

#[test]
fn test_mark_slot_dead_if_not_full() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let location = BlockLocation::Original;

    // Leave an empty slot
    let empty_slot = 0;

    // Insert a partial slot
    let partial_slot = 5;
    let (mut shreds, _) = make_slot_entries(partial_slot, partial_slot - 1, 100);
    assert!(shreds.len() > 1);
    shreds.pop();
    blockstore.insert_shreds(shreds, None, false).unwrap();
    assert!(!blockstore.meta(partial_slot).unwrap().unwrap().is_full());

    // Insert a full slot
    let full_slot = 10;
    let (shreds, _) = make_slot_entries(full_slot, full_slot - 1, 100);
    blockstore.insert_shreds(shreds, None, false).unwrap();
    assert!(blockstore.meta(full_slot).unwrap().unwrap().is_full());

    let mut shred_insertion_tracker = ShredInsertionTracker::new(1, blockstore.db.batch().unwrap());

    blockstore.mark_slot_dead_if_not_full(empty_slot, location, &mut shred_insertion_tracker);
    blockstore.mark_slot_dead_if_not_full(partial_slot, location, &mut shred_insertion_tracker);
    blockstore.mark_slot_dead_if_not_full(full_slot, location, &mut shred_insertion_tracker);
    // Commit the write batch so state changes can be read back
    blockstore
        .write_batch(shred_insertion_tracker.write_batch)
        .unwrap();
    assert!(blockstore.is_dead(empty_slot));
    assert!(blockstore.is_dead(partial_slot));
    assert!(!blockstore.is_dead(full_slot));
}

#[test]
fn test_slot_data_iterator() {
    // Construct the shreds
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let shreds_per_slot = 10;
    let slots = vec![2, 4, 8, 12];
    let all_shreds = make_chaining_slot_entries(&slots, shreds_per_slot, 0);
    let slot_8_shreds = all_shreds[2].0.clone();
    for (slot_shreds, _) in all_shreds {
        blockstore.insert_shreds(slot_shreds, None, false).unwrap();
    }

    // Slot doesn't exist, iterator should be empty
    let shred_iter = blockstore.slot_data_iterator(5, 0).unwrap();
    let result: Vec<_> = shred_iter.collect();
    assert_eq!(result, vec![]);

    // Test that the iterator for slot 8 contains what was inserted earlier
    let shred_iter = blockstore.slot_data_iterator(8, 0).unwrap();
    let result: Vec<Shred> = shred_iter
        .filter_map(|(_, bytes)| Shred::new_from_serialized_shred(bytes.to_vec()).ok())
        .collect();
    assert_eq!(result.len(), slot_8_shreds.len());
    assert_eq!(result, slot_8_shreds);
}

#[test]
fn test_set_roots() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let chained_slots = vec![0, 2, 4, 7, 12, 15];
    assert_eq!(blockstore.max_root(), 0);

    blockstore.set_roots(chained_slots.iter()).unwrap();

    assert_eq!(blockstore.max_root(), 15);

    for i in chained_slots {
        assert!(blockstore.is_root(i));
    }
}

#[test]
fn test_is_skipped() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let roots = [2, 4, 7, 12, 15];
    blockstore.set_roots(roots.iter()).unwrap();

    for i in 0..20 {
        if i < 2 || roots.contains(&i) || i > 15 {
            assert!(!blockstore.is_skipped(i));
        } else {
            assert!(blockstore.is_skipped(i));
        }
    }
}

#[test]
fn test_iter_bounds() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // slot 5 does not exist, iter should be ok and should be a noop
    assert_eq!(blockstore.slot_meta_iterator(5).unwrap().next(), None);
}

#[test]
fn test_get_completed_data_ranges() {
    let completed_data_end_indexes = [2, 4, 9, 11].iter().copied().collect();

    // Consumed is 1, which means we're missing shred with index 1, should return empty
    let start_index = 0;
    let consumed = 1;
    assert_eq!(
        Blockstore::get_completed_data_ranges(start_index, &completed_data_end_indexes, consumed),
        vec![]
    );

    let start_index = 0;
    let consumed = 3;
    assert_eq!(
        Blockstore::get_completed_data_ranges(start_index, &completed_data_end_indexes, consumed),
        vec![0..3]
    );

    // Test all possible ranges:
    //
    // `consumed == completed_data_end_indexes[j] + 1`, means we have all the shreds up to index
    // `completed_data_end_indexes[j] + 1`. Thus the completed data blocks is everything in the
    // range:
    // [start_index, completed_data_end_indexes[j]] ==
    // [completed_data_end_indexes[i], completed_data_end_indexes[j]],
    let completed_data_end_indexes: Vec<_> = completed_data_end_indexes.iter().collect();
    for i in 0..completed_data_end_indexes.len() {
        for j in i..completed_data_end_indexes.len() {
            let start_index = completed_data_end_indexes[i];
            let consumed = completed_data_end_indexes[j] + 1;
            // When start_index == completed_data_end_indexes[i], then that means
            // the shred with index == start_index is a single-shred data block,
            // so the start index is the end index for that data block.
            let expected = std::iter::once(start_index..start_index + 1)
                .chain(
                    completed_data_end_indexes[i..=j]
                        .windows(2)
                        .map(|end_indexes| end_indexes[0] + 1..end_indexes[1] + 1),
                )
                .collect::<Vec<_>>();

            let completed_data_end_indexes = completed_data_end_indexes.iter().copied().collect();
            assert_eq!(
                Blockstore::get_completed_data_ranges(
                    start_index,
                    &completed_data_end_indexes,
                    consumed
                ),
                expected
            );
        }
    }
}

#[test]
fn test_ranges_after_consumed() {
    let completed_data_end_indexes = [2, 4, 9, 11].iter().copied().collect();

    // start_index > consumed: out-of-order shred delivery during fast leader handover
    assert_eq!(
        Blockstore::get_completed_data_ranges(32, &completed_data_end_indexes, 3),
        vec![]
    );

    // start_index == consumed
    assert_eq!(
        Blockstore::get_completed_data_ranges(5, &completed_data_end_indexes, 5),
        vec![]
    );
}

#[test]
fn test_get_slot_entries_with_shred_count_corruption() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let num_ticks = 8;
    let entries = create_ticks(num_ticks, 0, Hash::default());
    let slot = 1;
    let shreds = entries_to_test_shreds(&entries, slot, 0, false, 0);
    let next_shred_index = shreds.len();
    blockstore
        .insert_shreds(shreds, None, false)
        .expect("Expected successful write of shreds");
    assert_eq!(
        blockstore.get_slot_entries(slot, 0).unwrap().len() as u64,
        num_ticks
    );

    // Insert an empty shred that won't deshred into entries

    let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 0).unwrap();
    let keypair = Keypair::new();
    let reed_solomon_cache = ReedSolomonCache::default();

    let shreds: Vec<Shred> = shredder
        .make_shreds_from_data_slice(
            &keypair,
            &[1, 1, 1],
            true,
            Hash::default(),
            next_shred_index as u32,
            next_shred_index as u32,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        )
        .unwrap()
        .take(DATA_SHREDS_PER_FEC_BLOCK)
        .collect();

    // With the corruption, nothing should be returned, even though an
    // earlier data block was valid
    blockstore
        .insert_shreds(shreds, None, false)
        .expect("Expected successful write of shreds");
    assert!(blockstore.get_slot_entries(slot, 0).is_err());
}

#[test]
fn test_no_insert_but_modify_slot_meta() {
    // This tests correctness of the SlotMeta in various cases in which a shred
    // that gets filtered out by checks
    let (shreds0, _) = make_slot_entries(0, 0, 200);
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Insert the first 5 shreds, we don't have a "is_last" shred yet
    blockstore
        .insert_shreds(shreds0[0..5].to_vec(), None, false)
        .unwrap();

    // Insert a repetitive shred for slot 's', should get ignored, but also
    // insert shreds that chains to 's', should see the update in the SlotMeta
    // for 's'.
    let (mut shreds2, _) = make_slot_entries(2, 0, 200);
    let (mut shreds3, _) = make_slot_entries(3, 0, 200);
    shreds2.push(shreds0[1].clone());
    shreds3.insert(0, shreds0[1].clone());
    blockstore.insert_shreds(shreds2, None, false).unwrap();
    let slot_meta = blockstore.meta(0).unwrap().unwrap();
    assert_eq!(slot_meta.next_slots, vec![2]);
    blockstore.insert_shreds(shreds3, None, false).unwrap();
    let slot_meta = blockstore.meta(0).unwrap().unwrap();
    assert_eq!(slot_meta.next_slots, vec![2, 3]);
}

#[test]
fn test_trusted_insert_shreds() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Make shred for slot 1
    let (shreds1, _) = make_slot_entries(1, 0, 1);
    let max_root = 100;

    blockstore.set_roots(std::iter::once(&max_root)).unwrap();

    // Insert will fail, slot < root
    blockstore
        .insert_shreds(shreds1[..].to_vec(), None, false)
        .unwrap();
    assert!(blockstore.get_data_shred(1, 0).unwrap().is_none());

    // Insert through trusted path will succeed
    blockstore
        .insert_shreds(shreds1[..].to_vec(), None, true)
        .unwrap();
    assert!(blockstore.get_data_shred(1, 0).unwrap().is_some());
}

#[test]
fn test_get_first_available_block() {
    let mint_total = 1_000_000_000_000;
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(mint_total);
    let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    assert_eq!(blockstore.get_first_available_block().unwrap(), 0);
    assert_eq!(blockstore.lowest_slot_with_genesis(), 0);
    assert_eq!(blockstore.lowest_slot(), 0);
    for slot in 1..4 {
        let entries = make_slot_entries_with_transactions(100);
        let shreds = entries_to_test_shreds(
            &entries,
            slot,
            slot - 1, // parent_slot
            true,     // is_full_slot
            0,        // version
        );
        blockstore.insert_shreds(shreds, None, false).unwrap();
        blockstore.set_roots([slot].iter()).unwrap();
    }
    assert_eq!(blockstore.get_first_available_block().unwrap(), 0);
    assert_eq!(blockstore.lowest_slot_with_genesis(), 0);
    assert_eq!(blockstore.lowest_slot(), 1);

    blockstore
        .purge_slots(0, 1, PurgeType::CompactionFilter)
        .unwrap();
    assert_eq!(blockstore.get_first_available_block().unwrap(), 3);
    assert_eq!(blockstore.lowest_slot_with_genesis(), 2);
    assert_eq!(blockstore.lowest_slot(), 2);
}

#[test]
fn test_get_rooted_block() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let entries = make_slot_entries_with_transactions(100);
    let blockhash = entries.last().unwrap().hash;

    // Insert a partially full slot
    let slot = 5;
    let shreds = entries_to_test_shreds(
        &entries,
        slot,
        slot - 1, // parent_slot
        false,    // is_full_slot
        0,        // version
    );
    blockstore.insert_shreds(shreds, None, false).unwrap();
    // Root the partially full slot and its parent
    blockstore.set_roots([slot - 1, slot].iter()).unwrap();
    // An empty slot will return an error even if the slot is rooted
    assert_matches!(
        blockstore.get_rooted_block(slot - 1, true),
        Err(BlockstoreError::SlotUnavailable)
    );
    // A partially full slot will return an error even if the slot is rooted
    assert_matches!(
        blockstore.get_rooted_block(slot, true),
        Err(BlockstoreError::SlotUnavailable)
    );

    // Insert a full slot
    let slot = 10;
    let shreds = entries_to_test_shreds(
        &entries,
        slot,
        slot - 1, // parent_slot
        true,     // is_full_slot
        0,        // version
    );
    blockstore.insert_shreds(shreds, None, false).unwrap();
    // Root both the full slot and its empty parent slot
    blockstore.set_roots([slot - 1, slot].iter()).unwrap();
    // A full slot will return an error if the previous blockhash is
    // required and the parent slot is empty
    assert_matches!(
        blockstore.get_rooted_block(slot, true),
        Err(BlockstoreError::ParentEntriesUnavailable)
    );

    // Insert a full slot with a partially full parent
    let slot = 15;
    let shreds = entries_to_test_shreds(
        &entries,
        slot - 1,
        slot - 2, // parent_slot
        false,    // is_full_slot
        0,        // version
    );
    blockstore.insert_shreds(shreds, None, false).unwrap();
    let shreds = entries_to_test_shreds(
        &entries,
        slot,
        slot - 1, // parent_slot
        true,     // is_full_slot
        0,        // version
    );
    blockstore.insert_shreds(shreds, None, false).unwrap();
    // Root both the full slot and its partially full parent slot
    blockstore.set_roots([slot - 1, slot].iter()).unwrap();
    // A full root will return an error if the previous blockhash is
    // required and the parent slot is partially full
    assert_matches!(
        blockstore.get_rooted_block(slot, true),
        Err(BlockstoreError::ParentEntriesUnavailable)
    );

    // Insert several successive full slots and populate the metadata
    let slot = 20;
    let shreds = entries_to_test_shreds(
        &entries,
        slot,
        slot - 1, // parent_slot
        true,     // is_full_slot
        0,        // version
    );
    blockstore.insert_shreds(shreds, None, false).unwrap();
    let shreds = entries_to_test_shreds(
        &entries,
        slot + 1,
        slot, // parent_slot
        true, // is_full_slot
        0,    // version
    );
    blockstore.insert_shreds(shreds, None, false).unwrap();
    let unrooted_shreds = entries_to_test_shreds(
        &entries,
        slot + 2,
        slot + 1, // parent_slot
        true,     // is_full_slot
        0,        // version
    );
    blockstore
        .insert_shreds(unrooted_shreds, None, false)
        .unwrap();
    blockstore.set_roots([slot, slot + 1].iter()).unwrap();
    let expected_transactions: Vec<VersionedTransactionWithStatusMeta> = entries
        .iter()
        .filter(|entry| !entry.is_tick())
        .cloned()
        .flat_map(|entry| entry.transactions)
        .map(|transaction| {
            let mut pre_balances: Vec<u64> = vec![];
            let mut post_balances: Vec<u64> = vec![];
            for i in 0..transaction.message.static_account_keys().len() {
                pre_balances.push(i as u64 * 10);
                post_balances.push(i as u64 * 11);
            }
            let compute_units_consumed = Some(12345);
            let cost_units = Some(6789);
            let signature = transaction.signatures[0];
            let status = TransactionStatusMeta {
                status: Ok(()),
                fee: 42,
                pre_balances: pre_balances.clone(),
                post_balances: post_balances.clone(),
                inner_instructions: Some(vec![]),
                log_messages: Some(vec![]),
                pre_token_balances: Some(vec![]),
                post_token_balances: Some(vec![]),
                rewards: Some(vec![]),
                loaded_addresses: LoadedAddresses::default(),
                return_data: Some(TransactionReturnData::default()),
                compute_units_consumed,
                cost_units,
            }
            .into();
            blockstore
                .transaction_status_cf
                .put_protobuf((signature, slot), &status)
                .unwrap();
            let status = TransactionStatusMeta {
                status: Ok(()),
                fee: 42,
                pre_balances: pre_balances.clone(),
                post_balances: post_balances.clone(),
                inner_instructions: Some(vec![]),
                log_messages: Some(vec![]),
                pre_token_balances: Some(vec![]),
                post_token_balances: Some(vec![]),
                rewards: Some(vec![]),
                loaded_addresses: LoadedAddresses::default(),
                return_data: Some(TransactionReturnData::default()),
                compute_units_consumed,
                cost_units,
            }
            .into();
            blockstore
                .transaction_status_cf
                .put_protobuf((signature, slot + 1), &status)
                .unwrap();
            let status = TransactionStatusMeta {
                status: Ok(()),
                fee: 42,
                pre_balances: pre_balances.clone(),
                post_balances: post_balances.clone(),
                inner_instructions: Some(vec![]),
                log_messages: Some(vec![]),
                pre_token_balances: Some(vec![]),
                post_token_balances: Some(vec![]),
                rewards: Some(vec![]),
                loaded_addresses: LoadedAddresses::default(),
                return_data: Some(TransactionReturnData::default()),
                compute_units_consumed,
                cost_units,
            }
            .into();
            blockstore
                .transaction_status_cf
                .put_protobuf((signature, slot + 2), &status)
                .unwrap();
            VersionedTransactionWithStatusMeta {
                transaction,
                meta: TransactionStatusMeta {
                    status: Ok(()),
                    fee: 42,
                    pre_balances,
                    post_balances,
                    inner_instructions: Some(vec![]),
                    log_messages: Some(vec![]),
                    pre_token_balances: Some(vec![]),
                    post_token_balances: Some(vec![]),
                    rewards: Some(vec![]),
                    loaded_addresses: LoadedAddresses::default(),
                    return_data: Some(TransactionReturnData::default()),
                    compute_units_consumed,
                    cost_units,
                },
            }
        })
        .collect();

    // Test for a slot where the parent is empty and previous blockhash is
    // not required
    let confirmed_block = blockstore.get_rooted_block(slot, false).unwrap();
    assert_eq!(confirmed_block.transactions.len(), 100);

    let expected_block = VersionedConfirmedBlock {
        transactions: expected_transactions.clone(),
        parent_slot: slot - 1,
        blockhash: blockhash.to_string(),
        previous_blockhash: Hash::default().to_string(),
        rewards: vec![],
        num_partitions: None,
        block_time: None,
        block_height: None,
    };
    assert_eq!(confirmed_block, expected_block);

    let confirmed_block = blockstore.get_rooted_block(slot + 1, true).unwrap();
    assert_eq!(confirmed_block.transactions.len(), 100);

    let mut expected_block = VersionedConfirmedBlock {
        transactions: expected_transactions.clone(),
        parent_slot: slot,
        blockhash: blockhash.to_string(),
        previous_blockhash: blockhash.to_string(),
        rewards: vec![],
        num_partitions: None,
        block_time: None,
        block_height: None,
    };
    assert_eq!(confirmed_block, expected_block);

    let not_root = blockstore.get_rooted_block(slot + 2, true).unwrap_err();
    assert_matches!(not_root, BlockstoreError::SlotNotRooted);

    let complete_block = blockstore.get_complete_block(slot + 2, true).unwrap();
    assert_eq!(complete_block.transactions.len(), 100);

    let mut expected_complete_block = VersionedConfirmedBlock {
        transactions: expected_transactions,
        parent_slot: slot + 1,
        blockhash: blockhash.to_string(),
        previous_blockhash: blockhash.to_string(),
        rewards: vec![],
        num_partitions: None,
        block_time: None,
        block_height: None,
    };
    assert_eq!(complete_block, expected_complete_block);

    // Test block_time & block_height return, if available
    let timestamp = 1_576_183_541;
    blockstore.blocktime_cf.put(slot + 1, &timestamp).unwrap();
    expected_block.block_time = Some(timestamp);
    let block_height = slot - 2;
    blockstore
        .block_height_cf
        .put(slot + 1, &block_height)
        .unwrap();
    expected_block.block_height = Some(block_height);

    let confirmed_block = blockstore.get_rooted_block(slot + 1, true).unwrap();
    assert_eq!(confirmed_block, expected_block);

    let timestamp = 1_576_183_542;
    blockstore.blocktime_cf.put(slot + 2, &timestamp).unwrap();
    expected_complete_block.block_time = Some(timestamp);
    let block_height = slot - 1;
    blockstore
        .block_height_cf
        .put(slot + 2, &block_height)
        .unwrap();
    expected_complete_block.block_height = Some(block_height);

    let complete_block = blockstore.get_complete_block(slot + 2, true).unwrap();
    assert_eq!(complete_block, expected_complete_block);
}

#[test]
fn test_persist_transaction_status() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let transaction_status_cf = &blockstore.transaction_status_cf;

    let pre_balances_vec = vec![1, 2, 3];
    let post_balances_vec = vec![3, 2, 1];
    let inner_instructions_vec = vec![InnerInstructions {
        index: 0,
        instructions: vec![InnerInstruction {
            instruction: CompiledInstruction::new(1, &(), vec![0]),
            stack_height: Some(2),
        }],
    }];
    let log_messages_vec = vec![String::from("Test message\n")];
    let pre_token_balances_vec = vec![];
    let post_token_balances_vec = vec![];
    let rewards_vec = vec![];
    let test_loaded_addresses = LoadedAddresses {
        writable: vec![Pubkey::new_unique()],
        readonly: vec![Pubkey::new_unique()],
    };
    let test_return_data = TransactionReturnData {
        program_id: Pubkey::new_unique(),
        data: vec![1, 2, 3],
    };
    let compute_units_consumed_1 = Some(3812649u64);
    let cost_units_1 = Some(1234);
    let compute_units_consumed_2 = Some(42u64);
    let cost_units_2 = Some(5678);

    // result not found
    assert!(
        transaction_status_cf
            .get_protobuf((Signature::default(), 0))
            .unwrap()
            .is_none()
    );

    // insert value
    let status = TransactionStatusMeta {
        status: solana_transaction_error::TransactionResult::<()>::Err(
            TransactionError::AccountNotFound,
        ),
        fee: 5u64,
        pre_balances: pre_balances_vec.clone(),
        post_balances: post_balances_vec.clone(),
        inner_instructions: Some(inner_instructions_vec.clone()),
        log_messages: Some(log_messages_vec.clone()),
        pre_token_balances: Some(pre_token_balances_vec.clone()),
        post_token_balances: Some(post_token_balances_vec.clone()),
        rewards: Some(rewards_vec.clone()),
        loaded_addresses: test_loaded_addresses.clone(),
        return_data: Some(test_return_data.clone()),
        compute_units_consumed: compute_units_consumed_1,
        cost_units: cost_units_1,
    }
    .into();
    assert!(
        transaction_status_cf
            .put_protobuf((Signature::default(), 0), &status)
            .is_ok()
    );

    // result found
    let TransactionStatusMeta {
        status,
        fee,
        pre_balances,
        post_balances,
        inner_instructions,
        log_messages,
        pre_token_balances,
        post_token_balances,
        rewards,
        loaded_addresses,
        return_data,
        compute_units_consumed,
        cost_units,
    } = transaction_status_cf
        .get_protobuf((Signature::default(), 0))
        .unwrap()
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(status, Err(TransactionError::AccountNotFound));
    assert_eq!(fee, 5u64);
    assert_eq!(pre_balances, pre_balances_vec);
    assert_eq!(post_balances, post_balances_vec);
    assert_eq!(inner_instructions.unwrap(), inner_instructions_vec);
    assert_eq!(log_messages.unwrap(), log_messages_vec);
    assert_eq!(pre_token_balances.unwrap(), pre_token_balances_vec);
    assert_eq!(post_token_balances.unwrap(), post_token_balances_vec);
    assert_eq!(rewards.unwrap(), rewards_vec);
    assert_eq!(loaded_addresses, test_loaded_addresses);
    assert_eq!(return_data.unwrap(), test_return_data);
    assert_eq!(compute_units_consumed, compute_units_consumed_1);
    assert_eq!(cost_units, cost_units_1);

    // insert value
    let status = TransactionStatusMeta {
        status: solana_transaction_error::TransactionResult::<()>::Ok(()),
        fee: 9u64,
        pre_balances: pre_balances_vec.clone(),
        post_balances: post_balances_vec.clone(),
        inner_instructions: Some(inner_instructions_vec.clone()),
        log_messages: Some(log_messages_vec.clone()),
        pre_token_balances: Some(pre_token_balances_vec.clone()),
        post_token_balances: Some(post_token_balances_vec.clone()),
        rewards: Some(rewards_vec.clone()),
        loaded_addresses: test_loaded_addresses.clone(),
        return_data: Some(test_return_data.clone()),
        compute_units_consumed: compute_units_consumed_2,
        cost_units: cost_units_2,
    }
    .into();
    assert!(
        transaction_status_cf
            .put_protobuf((Signature::from([2u8; 64]), 9), &status,)
            .is_ok()
    );

    // result found
    let TransactionStatusMeta {
        status,
        fee,
        pre_balances,
        post_balances,
        inner_instructions,
        log_messages,
        pre_token_balances,
        post_token_balances,
        rewards,
        loaded_addresses,
        return_data,
        compute_units_consumed,
        cost_units,
    } = transaction_status_cf
        .get_protobuf((Signature::from([2u8; 64]), 9))
        .unwrap()
        .unwrap()
        .try_into()
        .unwrap();

    // deserialize
    assert_eq!(status, Ok(()));
    assert_eq!(fee, 9u64);
    assert_eq!(pre_balances, pre_balances_vec);
    assert_eq!(post_balances, post_balances_vec);
    assert_eq!(inner_instructions.unwrap(), inner_instructions_vec);
    assert_eq!(log_messages.unwrap(), log_messages_vec);
    assert_eq!(pre_token_balances.unwrap(), pre_token_balances_vec);
    assert_eq!(post_token_balances.unwrap(), post_token_balances_vec);
    assert_eq!(rewards.unwrap(), rewards_vec);
    assert_eq!(loaded_addresses, test_loaded_addresses);
    assert_eq!(return_data.unwrap(), test_return_data);
    assert_eq!(compute_units_consumed, compute_units_consumed_2);
    assert_eq!(cost_units, cost_units_2);
}

#[test]
fn test_get_transaction_status() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let transaction_status_cf = &blockstore.transaction_status_cf;

    let pre_balances_vec = vec![1, 2, 3];
    let post_balances_vec = vec![3, 2, 1];
    let status = TransactionStatusMeta {
        status: solana_transaction_error::TransactionResult::<()>::Ok(()),
        fee: 42u64,
        pre_balances: pre_balances_vec,
        post_balances: post_balances_vec,
        inner_instructions: Some(vec![]),
        log_messages: Some(vec![]),
        pre_token_balances: Some(vec![]),
        post_token_balances: Some(vec![]),
        rewards: Some(vec![]),
        loaded_addresses: LoadedAddresses::default(),
        return_data: Some(TransactionReturnData::default()),
        compute_units_consumed: Some(42u64),
        cost_units: Some(1234),
    }
    .into();

    let signature1 = Signature::from([1u8; 64]);
    let signature2 = Signature::from([2u8; 64]);
    let signature3 = Signature::from([3u8; 64]);
    let signature4 = Signature::from([4u8; 64]);
    let signature5 = Signature::from([5u8; 64]);
    let signature6 = Signature::from([6u8; 64]);
    let signature7 = Signature::from([7u8; 64]);

    // Insert slots with fork
    //   0 (root)
    //  / \
    // 1  |
    //    2 (root)
    //    |
    //    3
    let meta0 = SlotMeta::new(0, Some(0));
    blockstore.meta_cf.put(0, &meta0).unwrap();
    let meta1 = SlotMeta::new(1, Some(0));
    blockstore.meta_cf.put(1, &meta1).unwrap();
    let meta2 = SlotMeta::new(2, Some(0));
    blockstore.meta_cf.put(2, &meta2).unwrap();
    let meta3 = SlotMeta::new(3, Some(2));
    blockstore.meta_cf.put(3, &meta3).unwrap();

    blockstore.set_roots([0, 2].iter()).unwrap();

    // Initialize statuses:
    //   signature2 in skipped slot and root,
    //   signature4 in skipped slot,
    //   signature5 in skipped slot and non-root,
    //   signature6 in skipped slot,
    //   signature5 extra entries
    transaction_status_cf
        .put_protobuf((signature2, 1), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature2, 2), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature4, 1), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature5, 1), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature5, 3), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature6, 1), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature5, 5), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature6, 3), &status)
        .unwrap();

    // Signature exists, root found
    if let (Some((slot, _status)), counter) = blockstore
        .get_transaction_status_with_counter(signature2, &[].into())
        .unwrap()
    {
        assert_eq!(slot, 2);
        assert_eq!(counter, 2);
    }

    // Signature exists, root found although not required
    if let (Some((slot, _status)), counter) = blockstore
        .get_transaction_status_with_counter(signature2, &[3].into())
        .unwrap()
    {
        assert_eq!(slot, 2);
        assert_eq!(counter, 2);
    }

    // Signature exists in skipped slot, no root found
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature4, &[].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 2);

    // Signature exists in skipped slot, no non-root found
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature4, &[3].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 2);

    // Signature exists, no root found
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature5, &[].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 4);

    // Signature exists, root not required
    if let (Some((slot, _status)), counter) = blockstore
        .get_transaction_status_with_counter(signature5, &[3].into())
        .unwrap()
    {
        assert_eq!(slot, 3);
        assert_eq!(counter, 2);
    }

    // Signature does not exist, smaller than existing entries
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature1, &[].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 1);

    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature1, &[3].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 1);

    // Signature does not exist, between existing entries
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature3, &[].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 1);

    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature3, &[3].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 1);

    // Signature does not exist, larger than existing entries
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature7, &[].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 0);

    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature7, &[3].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 0);
}

#[test]
fn test_get_transaction_status_with_old_data() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let transaction_status_cf = &blockstore.transaction_status_cf;

    let pre_balances_vec = vec![1, 2, 3];
    let post_balances_vec = vec![3, 2, 1];
    let status = TransactionStatusMeta {
        status: solana_transaction_error::TransactionResult::<()>::Ok(()),
        fee: 42u64,
        pre_balances: pre_balances_vec,
        post_balances: post_balances_vec,
        inner_instructions: Some(vec![]),
        log_messages: Some(vec![]),
        pre_token_balances: Some(vec![]),
        post_token_balances: Some(vec![]),
        rewards: Some(vec![]),
        loaded_addresses: LoadedAddresses::default(),
        return_data: Some(TransactionReturnData::default()),
        compute_units_consumed: Some(42u64),
        cost_units: Some(1234),
    }
    .into();

    let signature1 = Signature::from([1u8; 64]);
    let signature2 = Signature::from([2u8; 64]);
    let signature3 = Signature::from([3u8; 64]);
    let signature4 = Signature::from([4u8; 64]);
    let signature5 = Signature::from([5u8; 64]);
    let signature6 = Signature::from([6u8; 64]);

    // Insert slots with fork
    //   0 (root)
    //  / \
    // 1  |
    //    2 (root)
    //  / |
    // 3  |
    //    4 (root)
    //    |
    //    5
    let meta0 = SlotMeta::new(0, Some(0));
    blockstore.meta_cf.put(0, &meta0).unwrap();
    let meta1 = SlotMeta::new(1, Some(0));
    blockstore.meta_cf.put(1, &meta1).unwrap();
    let meta2 = SlotMeta::new(2, Some(0));
    blockstore.meta_cf.put(2, &meta2).unwrap();
    let meta3 = SlotMeta::new(3, Some(2));
    blockstore.meta_cf.put(3, &meta3).unwrap();
    let meta4 = SlotMeta::new(4, Some(2));
    blockstore.meta_cf.put(4, &meta4).unwrap();
    let meta5 = SlotMeta::new(5, Some(4));
    blockstore.meta_cf.put(5, &meta5).unwrap();

    blockstore.set_roots([0, 2, 4].iter()).unwrap();

    // Initialize statuses:
    //   signature1 in skipped slot (1) and root (2)
    //   signature2 in skipped slot (3) and root (4)
    //   signature3 in root (4)
    //   signature4 in non-root (5)
    //   signature5 extra entries
    transaction_status_cf
        .put_protobuf((signature1, 1), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature1, 2), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature2, 3), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature2, 4), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature3, 4), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature4, 5), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature5, 5), &status)
        .unwrap();

    // Signature exists
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature1, &[].into())
        .unwrap();
    let (slot, _status) = status.unwrap();
    assert_eq!(slot, 2);
    assert_eq!(counter, 2);

    // Signature exists
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature2, &[].into())
        .unwrap();
    let (slot, _status) = status.unwrap();
    assert_eq!(slot, 4);
    assert_eq!(counter, 2);

    // Signature exists
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature3, &[].into())
        .unwrap();
    let (slot, _status) = status.unwrap();
    assert_eq!(slot, 4);
    assert_eq!(counter, 1);

    // Signature does not exist (in a rooted block)
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature5, &[].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 1);

    // Signature does not exist
    let (status, counter) = blockstore
        .get_transaction_status_with_counter(signature6, &[].into())
        .unwrap();
    assert_eq!(status, None);
    assert_eq!(counter, 0);
}

fn do_test_lowest_cleanup_slot_and_special_cfs(simulate_blockstore_cleanup_service: bool) {
    agave_logger::setup();

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let transaction_status_cf = &blockstore.transaction_status_cf;

    let pre_balances_vec = vec![1, 2, 3];
    let post_balances_vec = vec![3, 2, 1];
    let status = TransactionStatusMeta {
        status: solana_transaction_error::TransactionResult::<()>::Ok(()),
        fee: 42u64,
        pre_balances: pre_balances_vec,
        post_balances: post_balances_vec,
        inner_instructions: Some(vec![]),
        log_messages: Some(vec![]),
        pre_token_balances: Some(vec![]),
        post_token_balances: Some(vec![]),
        rewards: Some(vec![]),
        loaded_addresses: LoadedAddresses::default(),
        return_data: Some(TransactionReturnData::default()),
        compute_units_consumed: Some(42u64),
        cost_units: Some(1234),
    }
    .into();

    let signature1 = Signature::from([2u8; 64]);
    let signature2 = Signature::from([3u8; 64]);

    // Insert rooted slots 0..=3 with no fork
    let meta0 = SlotMeta::new(0, Some(0));
    blockstore.meta_cf.put(0, &meta0).unwrap();
    let meta1 = SlotMeta::new(1, Some(0));
    blockstore.meta_cf.put(1, &meta1).unwrap();
    let meta2 = SlotMeta::new(2, Some(1));
    blockstore.meta_cf.put(2, &meta2).unwrap();
    let meta3 = SlotMeta::new(3, Some(2));
    blockstore.meta_cf.put(3, &meta3).unwrap();

    blockstore.set_roots([0, 1, 2, 3].iter()).unwrap();

    let lowest_cleanup_slot = 1;
    let lowest_available_slot = lowest_cleanup_slot + 1;

    transaction_status_cf
        .put_protobuf((signature1, lowest_cleanup_slot), &status)
        .unwrap();

    transaction_status_cf
        .put_protobuf((signature2, lowest_available_slot), &status)
        .unwrap();

    let address0 = solana_pubkey::new_rand();
    let address1 = solana_pubkey::new_rand();
    blockstore
        .write_transaction_status(
            lowest_cleanup_slot,
            signature1,
            vec![(&address0, true)].into_iter(),
            TransactionStatusMeta::default(),
            0,
        )
        .unwrap();
    blockstore
        .write_transaction_status(
            lowest_available_slot,
            signature2,
            vec![(&address1, true)].into_iter(),
            TransactionStatusMeta::default(),
            0,
        )
        .unwrap();

    let check_for_missing = || {
        (
            blockstore
                .get_transaction_status_with_counter(signature1, &[].into())
                .unwrap()
                .0
                .is_none(),
            blockstore
                .find_address_signatures_for_slot(address0, lowest_cleanup_slot)
                .unwrap()
                .is_empty(),
        )
    };

    let assert_existing_always = || {
        let are_existing_always = (
            blockstore
                .get_transaction_status_with_counter(signature2, &[].into())
                .unwrap()
                .0
                .is_some(),
            !blockstore
                .find_address_signatures_for_slot(address1, lowest_available_slot)
                .unwrap()
                .is_empty(),
        );
        assert_eq!(are_existing_always, (true, true));
    };

    let are_missing = check_for_missing();
    // should never be missing before the conditional compaction & simulation...
    assert_eq!(are_missing, (false, false));
    assert_existing_always();

    if simulate_blockstore_cleanup_service {
        *blockstore.lowest_cleanup_slot.write().unwrap() = lowest_cleanup_slot;
        blockstore
            .purge_slots(0, lowest_cleanup_slot, PurgeType::CompactionFilter)
            .unwrap();
    }

    let are_missing = check_for_missing();
    if simulate_blockstore_cleanup_service {
        // ... when either simulation (or both) is effective, we should observe to be missing
        // consistently
        assert_eq!(are_missing, (true, true));
    } else {
        // ... otherwise, we should observe to be existing...
        assert_eq!(are_missing, (false, false));
    }
    assert_existing_always();
}

#[test]
fn test_lowest_cleanup_slot_and_special_cfs_with_blockstore_cleanup_service_simulation() {
    do_test_lowest_cleanup_slot_and_special_cfs(true);
}

#[test]
fn test_lowest_cleanup_slot_and_special_cfs_without_blockstore_cleanup_service_simulation() {
    do_test_lowest_cleanup_slot_and_special_cfs(false);
}

#[test]
fn test_get_rooted_transaction() {
    let slot = 2;
    let entries = make_slot_entries_with_transactions(5);
    let shreds = entries_to_test_shreds(
        &entries,
        slot,
        slot - 1, // parent_slot
        true,     // is_full_slot
        0,        // version
    );
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    blockstore.insert_shreds(shreds, None, false).unwrap();
    blockstore.set_roots([slot - 1, slot].iter()).unwrap();

    let expected_transactions: Vec<VersionedTransactionWithStatusMeta> = entries
        .iter()
        .filter(|entry| !entry.is_tick())
        .cloned()
        .flat_map(|entry| entry.transactions)
        .map(|transaction| {
            let mut pre_balances: Vec<u64> = vec![];
            let mut post_balances: Vec<u64> = vec![];
            for i in 0..transaction.message.static_account_keys().len() {
                pre_balances.push(i as u64 * 10);
                post_balances.push(i as u64 * 11);
            }
            let inner_instructions = Some(vec![InnerInstructions {
                index: 0,
                instructions: vec![InnerInstruction {
                    instruction: CompiledInstruction::new(1, &(), vec![0]),
                    stack_height: Some(2),
                }],
            }]);
            let log_messages = Some(vec![String::from("Test message\n")]);
            let pre_token_balances = Some(vec![]);
            let post_token_balances = Some(vec![]);
            let rewards = Some(vec![]);
            let signature = transaction.signatures[0];
            let return_data = Some(TransactionReturnData {
                program_id: Pubkey::new_unique(),
                data: vec![1, 2, 3],
            });
            let status = TransactionStatusMeta {
                status: Ok(()),
                fee: 42,
                pre_balances: pre_balances.clone(),
                post_balances: post_balances.clone(),
                inner_instructions: inner_instructions.clone(),
                log_messages: log_messages.clone(),
                pre_token_balances: pre_token_balances.clone(),
                post_token_balances: post_token_balances.clone(),
                rewards: rewards.clone(),
                loaded_addresses: LoadedAddresses::default(),
                return_data: return_data.clone(),
                compute_units_consumed: Some(42),
                cost_units: Some(1234),
            }
            .into();
            blockstore
                .transaction_status_cf
                .put_protobuf((signature, slot), &status)
                .unwrap();
            VersionedTransactionWithStatusMeta {
                transaction,
                meta: TransactionStatusMeta {
                    status: Ok(()),
                    fee: 42,
                    pre_balances,
                    post_balances,
                    inner_instructions,
                    log_messages,
                    pre_token_balances,
                    post_token_balances,
                    rewards,
                    loaded_addresses: LoadedAddresses::default(),
                    return_data,
                    compute_units_consumed: Some(42),
                    cost_units: Some(1234),
                },
            }
        })
        .collect();

    for (index, tx_with_meta) in expected_transactions.clone().into_iter().enumerate() {
        let signature = tx_with_meta.transaction.signatures[0];
        assert_eq!(
            blockstore.get_rooted_transaction(signature).unwrap(),
            Some(ConfirmedTransactionWithStatusMeta {
                slot,
                tx_with_meta: TransactionWithStatusMeta::Complete(tx_with_meta.clone()),
                block_time: None,
                index: index as u32,
            })
        );
        assert_eq!(
            blockstore
                .get_complete_transaction(signature, slot + 1)
                .unwrap(),
            Some(ConfirmedTransactionWithStatusMeta {
                slot,
                tx_with_meta: TransactionWithStatusMeta::Complete(tx_with_meta),
                block_time: None,
                index: index as u32,
            })
        );
    }

    blockstore
        .purge_slots(0, slot, PurgeType::CompactionFilter)
        .unwrap();
    *blockstore.lowest_cleanup_slot.write().unwrap() = slot;
    for VersionedTransactionWithStatusMeta { transaction, .. } in expected_transactions {
        let signature = transaction.signatures[0];
        assert_eq!(blockstore.get_rooted_transaction(signature).unwrap(), None);
        assert_eq!(
            blockstore
                .get_complete_transaction(signature, slot + 1)
                .unwrap(),
            None,
        );
    }
}

#[test]
fn test_get_complete_transaction() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 2;
    let entries = make_slot_entries_with_transactions(5);
    let shreds = entries_to_test_shreds(
        &entries,
        slot,
        slot - 1, // parent_slot
        true,     // is_full_slot
        0,        // version
    );
    blockstore.insert_shreds(shreds, None, false).unwrap();

    let expected_transactions: Vec<VersionedTransactionWithStatusMeta> = entries
        .iter()
        .filter(|entry| !entry.is_tick())
        .cloned()
        .flat_map(|entry| entry.transactions)
        .map(|transaction| {
            let mut pre_balances: Vec<u64> = vec![];
            let mut post_balances: Vec<u64> = vec![];
            for i in 0..transaction.message.static_account_keys().len() {
                pre_balances.push(i as u64 * 10);
                post_balances.push(i as u64 * 11);
            }
            let inner_instructions = Some(vec![InnerInstructions {
                index: 0,
                instructions: vec![InnerInstruction {
                    instruction: CompiledInstruction::new(1, &(), vec![0]),
                    stack_height: Some(2),
                }],
            }]);
            let log_messages = Some(vec![String::from("Test message\n")]);
            let pre_token_balances = Some(vec![]);
            let post_token_balances = Some(vec![]);
            let rewards = Some(vec![]);
            let return_data = Some(TransactionReturnData {
                program_id: Pubkey::new_unique(),
                data: vec![1, 2, 3],
            });
            let signature = transaction.signatures[0];
            let status = TransactionStatusMeta {
                status: Ok(()),
                fee: 42,
                pre_balances: pre_balances.clone(),
                post_balances: post_balances.clone(),
                inner_instructions: inner_instructions.clone(),
                log_messages: log_messages.clone(),
                pre_token_balances: pre_token_balances.clone(),
                post_token_balances: post_token_balances.clone(),
                rewards: rewards.clone(),
                loaded_addresses: LoadedAddresses::default(),
                return_data: return_data.clone(),
                compute_units_consumed: Some(42u64),
                cost_units: Some(1234),
            }
            .into();
            blockstore
                .transaction_status_cf
                .put_protobuf((signature, slot), &status)
                .unwrap();
            VersionedTransactionWithStatusMeta {
                transaction,
                meta: TransactionStatusMeta {
                    status: Ok(()),
                    fee: 42,
                    pre_balances,
                    post_balances,
                    inner_instructions,
                    log_messages,
                    pre_token_balances,
                    post_token_balances,
                    rewards,
                    loaded_addresses: LoadedAddresses::default(),
                    return_data,
                    compute_units_consumed: Some(42u64),
                    cost_units: Some(1234),
                },
            }
        })
        .collect();

    for (index, tx_with_meta) in expected_transactions.clone().into_iter().enumerate() {
        let signature = tx_with_meta.transaction.signatures[0];
        assert_eq!(
            blockstore
                .get_complete_transaction(signature, slot)
                .unwrap(),
            Some(ConfirmedTransactionWithStatusMeta {
                slot,
                tx_with_meta: TransactionWithStatusMeta::Complete(tx_with_meta),
                block_time: None,
                index: index as u32,
            })
        );
        assert_eq!(blockstore.get_rooted_transaction(signature).unwrap(), None);
    }

    blockstore
        .purge_slots(0, slot, PurgeType::CompactionFilter)
        .unwrap();
    *blockstore.lowest_cleanup_slot.write().unwrap() = slot;
    for VersionedTransactionWithStatusMeta { transaction, .. } in expected_transactions {
        let signature = transaction.signatures[0];
        assert_eq!(
            blockstore
                .get_complete_transaction(signature, slot)
                .unwrap(),
            None,
        );
        assert_eq!(blockstore.get_rooted_transaction(signature).unwrap(), None,);
    }
}

#[test]
fn test_empty_transaction_status() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    blockstore.set_roots(std::iter::once(&0)).unwrap();
    assert_eq!(
        blockstore
            .get_rooted_transaction(Signature::default())
            .unwrap(),
        None
    );
}

#[test]
fn test_find_address_signatures_for_slot() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let address0 = solana_pubkey::new_rand();
    let address1 = solana_pubkey::new_rand();

    let slot1 = 1;
    for x in 1..5 {
        let signature = Signature::from([x; 64]);
        blockstore
            .write_transaction_status(
                slot1,
                signature,
                vec![(&address0, true), (&address1, false)].into_iter(),
                TransactionStatusMeta::default(),
                x as usize,
            )
            .unwrap();
    }
    let slot2 = 2;
    for x in 5..7 {
        let signature = Signature::from([x; 64]);
        blockstore
            .write_transaction_status(
                slot2,
                signature,
                vec![(&address0, true), (&address1, false)].into_iter(),
                TransactionStatusMeta::default(),
                x as usize,
            )
            .unwrap();
    }
    for x in 7..9 {
        let signature = Signature::from([x; 64]);
        blockstore
            .write_transaction_status(
                slot2,
                signature,
                vec![(&address0, true), (&address1, false)].into_iter(),
                TransactionStatusMeta::default(),
                x as usize,
            )
            .unwrap();
    }
    let slot3 = 3;
    for x in 9..13 {
        let signature = Signature::from([x; 64]);
        blockstore
            .write_transaction_status(
                slot3,
                signature,
                vec![(&address0, true), (&address1, false)].into_iter(),
                TransactionStatusMeta::default(),
                x as usize,
            )
            .unwrap();
    }
    blockstore.set_roots(std::iter::once(&slot1)).unwrap();

    let slot1_signatures = blockstore
        .find_address_signatures_for_slot(address0, 1)
        .unwrap();
    for (i, (slot, signature, index)) in slot1_signatures.iter().enumerate() {
        assert_eq!(*slot, slot1);
        assert_eq!(*signature, Signature::from([i as u8 + 1; 64]));
        assert_eq!(*index, (i + 1) as u32);
    }

    let slot2_signatures = blockstore
        .find_address_signatures_for_slot(address0, 2)
        .unwrap();
    for (i, (slot, signature, index)) in slot2_signatures.iter().enumerate() {
        assert_eq!(*slot, slot2);
        assert_eq!(*signature, Signature::from([i as u8 + 5; 64]));
        assert_eq!(*index, (i + 5) as u32);
    }

    let slot3_signatures = blockstore
        .find_address_signatures_for_slot(address0, 3)
        .unwrap();
    for (i, (slot, signature, index)) in slot3_signatures.iter().enumerate() {
        assert_eq!(*slot, slot3);
        assert_eq!(*signature, Signature::from([i as u8 + 9; 64]));
        assert_eq!(*index, (i + 9) as u32);
    }
}

#[test]
fn test_get_confirmed_signatures_for_address2() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let (shreds, _) = make_slot_entries(1, 0, 4);
    blockstore.insert_shreds(shreds, None, false).unwrap();

    fn make_slot_entries_with_transaction_addresses(addresses: &[Pubkey]) -> Vec<Entry> {
        let mut entries: Vec<Entry> = Vec::new();
        for address in addresses {
            let transaction = Transaction::new_with_compiled_instructions(
                &[&Keypair::new()],
                &[*address],
                Hash::default(),
                vec![solana_pubkey::new_rand()],
                vec![CompiledInstruction::new(1, &(), vec![0])],
            );
            entries.push(next_entry_mut(&mut Hash::default(), 0, vec![transaction]));
            let mut tick = create_ticks(1, 0, hash(&bincode::serialize(address).unwrap()));
            entries.append(&mut tick);
        }
        entries
    }

    let address0 = solana_pubkey::new_rand();
    let address1 = solana_pubkey::new_rand();

    for slot in 2..=8 {
        let entries =
            make_slot_entries_with_transaction_addresses(&[address0, address1, address0, address1]);
        let shreds = entries_to_test_shreds(
            &entries,
            slot,
            slot - 1, // parent_slot
            true,     // is_full_slot
            0,        // version
        );
        blockstore.insert_shreds(shreds, None, false).unwrap();

        let mut counter = 0;
        for entry in entries.into_iter() {
            for transaction in entry.transactions {
                assert_eq!(transaction.signatures.len(), 1);
                blockstore
                    .write_transaction_status(
                        slot,
                        transaction.signatures[0],
                        transaction
                            .message
                            .static_account_keys()
                            .iter()
                            .map(|key| (key, true)),
                        TransactionStatusMeta::default(),
                        counter,
                    )
                    .unwrap();
                counter += 1;
            }
        }
    }

    // Add 2 slots that both descend from slot 8
    for slot in 9..=10 {
        let entries =
            make_slot_entries_with_transaction_addresses(&[address0, address1, address0, address1]);
        let shreds = entries_to_test_shreds(&entries, slot, 8, true, 0);
        blockstore.insert_shreds(shreds, None, false).unwrap();

        let mut counter = 0;
        for entry in entries.into_iter() {
            for transaction in entry.transactions {
                assert_eq!(transaction.signatures.len(), 1);
                blockstore
                    .write_transaction_status(
                        slot,
                        transaction.signatures[0],
                        transaction
                            .message
                            .static_account_keys()
                            .iter()
                            .map(|key| (key, true)),
                        TransactionStatusMeta::default(),
                        counter,
                    )
                    .unwrap();
                counter += 1;
            }
        }
    }

    // Leave one slot unrooted to test only returns confirmed signatures
    blockstore.set_roots([1, 2, 4, 5, 6, 7, 8].iter()).unwrap();
    let highest_super_majority_root = 8;

    // Fetch all rooted signatures for address 0 at once...
    let sig_infos = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_super_majority_root,
            None,
            None,
            usize::MAX,
        )
        .unwrap();
    assert!(sig_infos.found_before);
    let all0 = sig_infos.infos;
    assert_eq!(all0.len(), 12);

    // Fetch all rooted signatures for address 1 at once...
    let all1 = blockstore
        .get_confirmed_signatures_for_address2(
            address1,
            highest_super_majority_root,
            None,
            None,
            usize::MAX,
        )
        .unwrap()
        .infos;
    assert_eq!(all1.len(), 12);

    // Fetch all signatures for address 0 individually
    for i in 0..all0.len() {
        let sig_infos = blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_super_majority_root,
                if i == 0 {
                    None
                } else {
                    Some(all0[i - 1].signature)
                },
                None,
                1,
            )
            .unwrap();
        assert!(sig_infos.found_before);
        let results = sig_infos.infos;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], all0[i], "Unexpected result for {i}");
    }
    // Fetch all signatures for address 0 individually using `until`
    for i in 0..all0.len() {
        let results = blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_super_majority_root,
                if i == 0 {
                    None
                } else {
                    Some(all0[i - 1].signature)
                },
                if i == all0.len() - 1 || i == all0.len() {
                    None
                } else {
                    Some(all0[i + 1].signature)
                },
                10,
            )
            .unwrap()
            .infos;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], all0[i], "Unexpected result for {i}");
    }

    let sig_infos = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_super_majority_root,
            Some(all0[all0.len() - 1].signature),
            None,
            1,
        )
        .unwrap();
    assert!(sig_infos.found_before);
    assert!(sig_infos.infos.is_empty());

    assert!(
        blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_super_majority_root,
                None,
                Some(all0[0].signature),
                2,
            )
            .unwrap()
            .infos
            .is_empty()
    );

    // Fetch all signatures for address 0, three at a time
    assert!(all0.len().is_multiple_of(3));
    for i in (0..all0.len()).step_by(3) {
        let results = blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_super_majority_root,
                if i == 0 {
                    None
                } else {
                    Some(all0[i - 1].signature)
                },
                None,
                3,
            )
            .unwrap()
            .infos;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], all0[i]);
        assert_eq!(results[1], all0[i + 1]);
        assert_eq!(results[2], all0[i + 2]);
    }

    // Ensure that the signatures within a slot are reverse ordered by occurrence in block
    for i in (0..all1.len()).step_by(2) {
        let results = blockstore
            .get_confirmed_signatures_for_address2(
                address1,
                highest_super_majority_root,
                if i == 0 {
                    None
                } else {
                    Some(all1[i - 1].signature)
                },
                None,
                2,
            )
            .unwrap()
            .infos;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].slot, results[1].slot);
        assert_eq!(results[0], all1[i]);
        assert_eq!(results[1], all1[i + 1]);
    }

    // A search for address 0 with `before` and/or `until` signatures from address1 should also work
    let sig_infos = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_super_majority_root,
            Some(all1[0].signature),
            None,
            usize::MAX,
        )
        .unwrap();
    assert!(sig_infos.found_before);
    let results = sig_infos.infos;
    // The exact number of results returned is variable, based on the sort order of the
    // random signatures that are generated
    assert!(!results.is_empty());

    let results2 = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_super_majority_root,
            Some(all1[0].signature),
            Some(all1[4].signature),
            usize::MAX,
        )
        .unwrap()
        .infos;
    assert!(results2.len() < results.len());

    // Duplicate all tests using confirmed signatures
    let highest_confirmed_slot = 10;

    // Fetch all signatures for address 0 at once...
    let all0 = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_confirmed_slot,
            None,
            None,
            usize::MAX,
        )
        .unwrap()
        .infos;
    assert_eq!(all0.len(), 14);

    // Fetch all signatures for address 1 at once...
    let all1 = blockstore
        .get_confirmed_signatures_for_address2(
            address1,
            highest_confirmed_slot,
            None,
            None,
            usize::MAX,
        )
        .unwrap()
        .infos;
    assert_eq!(all1.len(), 14);

    // Fetch all signatures for address 0 individually
    for i in 0..all0.len() {
        let results = blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_confirmed_slot,
                if i == 0 {
                    None
                } else {
                    Some(all0[i - 1].signature)
                },
                None,
                1,
            )
            .unwrap()
            .infos;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], all0[i], "Unexpected result for {i}");
    }
    // Fetch all signatures for address 0 individually using `until`
    for i in 0..all0.len() {
        let results = blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_confirmed_slot,
                if i == 0 {
                    None
                } else {
                    Some(all0[i - 1].signature)
                },
                if i == all0.len() - 1 || i == all0.len() {
                    None
                } else {
                    Some(all0[i + 1].signature)
                },
                10,
            )
            .unwrap()
            .infos;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], all0[i], "Unexpected result for {i}");
    }

    assert!(
        blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_confirmed_slot,
                Some(all0[all0.len() - 1].signature),
                None,
                1,
            )
            .unwrap()
            .infos
            .is_empty()
    );

    assert!(
        blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_confirmed_slot,
                None,
                Some(all0[0].signature),
                2,
            )
            .unwrap()
            .infos
            .is_empty()
    );

    // Fetch all signatures for address 0, three at a time
    assert!(all0.len() % 3 == 2);
    for i in (0..all0.len()).step_by(3) {
        let results = blockstore
            .get_confirmed_signatures_for_address2(
                address0,
                highest_confirmed_slot,
                if i == 0 {
                    None
                } else {
                    Some(all0[i - 1].signature)
                },
                None,
                3,
            )
            .unwrap()
            .infos;
        if i < 12 {
            assert_eq!(results.len(), 3);
            assert_eq!(results[2], all0[i + 2]);
        } else {
            assert_eq!(results.len(), 2);
        }
        assert_eq!(results[0], all0[i]);
        assert_eq!(results[1], all0[i + 1]);
    }

    // Ensure that the signatures within a slot are reverse ordered by occurrence in block
    for i in (0..all1.len()).step_by(2) {
        let results = blockstore
            .get_confirmed_signatures_for_address2(
                address1,
                highest_confirmed_slot,
                if i == 0 {
                    None
                } else {
                    Some(all1[i - 1].signature)
                },
                None,
                2,
            )
            .unwrap()
            .infos;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].slot, results[1].slot);
        assert_eq!(results[0], all1[i]);
        assert_eq!(results[1], all1[i + 1]);
    }

    // A search for address 0 with `before` and/or `until` signatures from address1 should also work
    let results = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_confirmed_slot,
            Some(all1[0].signature),
            None,
            usize::MAX,
        )
        .unwrap()
        .infos;
    // The exact number of results returned is variable, based on the sort order of the
    // random signatures that are generated
    assert!(!results.is_empty());

    let results2 = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_confirmed_slot,
            Some(all1[0].signature),
            Some(all1[4].signature),
            usize::MAX,
        )
        .unwrap()
        .infos;
    assert!(results2.len() < results.len());

    // Remove signature
    blockstore
        .address_signatures_cf
        .delete((address0, 2, 0, all0[0].signature))
        .unwrap();
    let sig_infos = blockstore
        .get_confirmed_signatures_for_address2(
            address0,
            highest_super_majority_root,
            Some(all0[0].signature),
            None,
            usize::MAX,
        )
        .unwrap();
    assert!(!sig_infos.found_before);
    assert!(sig_infos.infos.is_empty());
}

#[test]
fn test_map_transactions_to_statuses() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let transaction_status_cf = &blockstore.transaction_status_cf;

    let slot = 0;
    let mut transactions: Vec<VersionedTransaction> = vec![];
    for x in 0..4 {
        let transaction = Transaction::new_with_compiled_instructions(
            &[&Keypair::new()],
            &[solana_pubkey::new_rand()],
            Hash::default(),
            vec![solana_pubkey::new_rand()],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        let status = TransactionStatusMeta {
            status: solana_transaction_error::TransactionResult::<()>::Err(
                TransactionError::AccountNotFound,
            ),
            fee: x,
            pre_balances: vec![],
            post_balances: vec![],
            inner_instructions: Some(vec![]),
            log_messages: Some(vec![]),
            pre_token_balances: Some(vec![]),
            post_token_balances: Some(vec![]),
            rewards: Some(vec![]),
            loaded_addresses: LoadedAddresses::default(),
            return_data: Some(TransactionReturnData::default()),
            compute_units_consumed: None,
            cost_units: None,
        }
        .into();
        transaction_status_cf
            .put_protobuf((transaction.signatures[0], slot), &status)
            .unwrap();
        transactions.push(transaction.into());
    }

    let map_result =
        blockstore.map_transactions_to_statuses(slot, transactions.clone().into_iter());
    assert!(map_result.is_ok());
    let map = map_result.unwrap();
    assert_eq!(map.len(), 4);
    for (x, m) in map.iter().enumerate() {
        assert_eq!(m.meta.fee, x as u64);
    }

    // Push transaction that will not have matching status, as a test case
    transactions.push(
        Transaction::new_with_compiled_instructions(
            &[&Keypair::new()],
            &[solana_pubkey::new_rand()],
            Hash::default(),
            vec![solana_pubkey::new_rand()],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        )
        .into(),
    );

    let map_result =
        blockstore.map_transactions_to_statuses(slot, transactions.clone().into_iter());
    assert_matches!(map_result, Err(BlockstoreError::MissingTransactionMetadata));
}

#[test]
fn test_write_perf_samples() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let num_entries: usize = 10;
    let mut perf_samples: Vec<(Slot, PerfSample)> = vec![];
    for x in 1..num_entries + 1 {
        let slot = x as u64 * 50;
        let sample = PerfSample {
            num_transactions: 1000 + x as u64,
            num_slots: 50,
            sample_period_secs: 20,
            num_non_vote_transactions: 300 + x as u64,
        };

        blockstore.write_perf_sample(slot, &sample).unwrap();
        perf_samples.push((slot, sample));
    }

    for x in 0..num_entries {
        let mut expected_samples = perf_samples[num_entries - 1 - x..].to_vec();
        expected_samples.sort_by_key(|b| cmp::Reverse(b.0));
        assert_eq!(
            blockstore.get_recent_perf_samples(x + 1).unwrap(),
            expected_samples
        );
    }
}

#[test]
fn test_lowest_slot() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    assert_eq!(blockstore.lowest_slot(), 0);

    for slot in 0..10 {
        let (shreds, _) = make_slot_entries(slot, 0, 1);
        blockstore.insert_shreds(shreds, None, false).unwrap();
    }
    assert_eq!(blockstore.lowest_slot(), 1);
    blockstore.purge_slots(0, 5, PurgeType::Exact).unwrap();
    assert_eq!(blockstore.lowest_slot(), 6);
}

#[test]
fn test_highest_slot() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    assert_eq!(blockstore.highest_slot().unwrap(), None);

    for slot in 0..10 {
        let (shreds, _) = make_slot_entries(slot, 0, 1);
        blockstore.insert_shreds(shreds, None, false).unwrap();
        assert_eq!(blockstore.highest_slot().unwrap(), Some(slot));
    }
    blockstore.purge_slots(5, 10, PurgeType::Exact).unwrap();
    assert_eq!(blockstore.highest_slot().unwrap(), Some(4));

    blockstore.purge_slots(0, 4, PurgeType::Exact).unwrap();
    assert_eq!(blockstore.highest_slot().unwrap(), None);
}

#[test]
fn test_recovery() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 1;
    let (data_shreds, coding_shreds, leader_schedule_cache) = setup_erasure_shreds(slot, 0, 100);
    let genesis_config = create_genesis_config(2).genesis_config;
    let root_bank = Arc::new(Bank::new_for_tests(&genesis_config));

    let (dummy_retransmit_sender, _) = EvictingSender::new_bounded(0);
    let coding_shreds = coding_shreds.into_iter().map(|shred| {
        (
            Cow::Owned(shred),
            /*is_repaired:*/ false,
            BlockLocation::Original,
        )
    });
    blockstore
        .do_insert_shreds(
            coding_shreds,
            Some(&leader_schedule_cache),
            false, // is_trusted
            Some(&mut ShredRecoveryContext::new(
                ReedSolomonCache::default(),
                dummy_retransmit_sender,
                root_bank,
                0, // shred_version
            )),
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();
    let shred_bufs: Vec<_> = data_shreds.iter().map(Shred::payload).cloned().collect();

    // Check all the data shreds were recovered
    for (s, buf) in data_shreds.iter().zip(shred_bufs) {
        assert_eq!(
            blockstore
                .get_data_shred(s.slot(), s.index() as u64)
                .unwrap()
                .unwrap(),
            buf.as_ref(),
        );
    }

    verify_index_integrity(&blockstore, slot);
}

#[test]
fn test_skip_alt_recovery() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 1;
    let (data_shreds, coding_shreds, leader_schedule_cache) = setup_erasure_shreds(slot, 0, 100);
    let data_shred = data_shreds[0].clone();
    let data_shred_index = u64::from(data_shred.index());

    // First populate Original erasure metadata through a coding shred. A
    // later Alternate-column block-id repair insertion for the same FEC set
    // will load this erasure meta without touching the Original index in the
    // current batch.
    blockstore
        .do_insert_shreds(
            std::iter::once((
                Cow::Owned(coding_shreds[0].clone()),
                /*is_repaired:*/ false,
                BlockLocation::Original,
            )),
            Some(&leader_schedule_cache),
            false, // is_trusted
            None,
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();

    let genesis_config = create_genesis_config(2).genesis_config;
    let root_bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let (dummy_retransmit_sender, _) = EvictingSender::new_bounded(0);
    let alternate_location = BlockLocation::Alternate {
        block_id: Hash::new_unique(),
    };
    let mut metrics = BlockstoreInsertionMetrics::default();

    blockstore
        .do_insert_shreds(
            std::iter::once((
                Cow::Owned(data_shred),
                /*is_repaired:*/ true,
                alternate_location,
            )),
            Some(&leader_schedule_cache),
            false, // is_trusted
            Some(&mut ShredRecoveryContext::new(
                ReedSolomonCache::default(),
                dummy_retransmit_sender,
                root_bank,
                0, // shred_version
            )),
            &mut metrics,
        )
        .unwrap();

    assert_eq!(metrics.num_recovered, 0);
    assert!(
        blockstore
            .get_index_from_location(slot, alternate_location)
            .unwrap()
            .unwrap()
            .data()
            .contains(data_shred_index)
    );
    assert!(
        blockstore
            .get_data_shred(slot, data_shred_index)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_recovery_discards_unexpected_data_complete_shreds() {
    const DATA_SHRED_FLAGS_OFFSET: usize = 85;

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let genesis_config = create_genesis_config(2).genesis_config;
    let mut root_bank = Bank::new_for_tests(&genesis_config);
    root_bank.activate_feature(&discard_unexpected_data_complete_shreds::id());
    let root_bank = Arc::new(root_bank);
    let slot = root_bank.get_slots_in_epoch(root_bank.epoch());
    let reed_solomon_cache = ReedSolomonCache::default();
    let (data_shreds, coding_shreds, leader_keypair, leader_schedule_cache) =
        setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot_and_keypair(
            slot,
            0,
            100,
            0,
            Hash::new_from_array(rand::rng().random()),
            false, // is_last_in_slot
        );
    assert!(data_shreds.len() >= DATA_SHREDS_PER_FEC_BLOCK);
    assert!(coding_shreds.len() >= DATA_SHREDS_PER_FEC_BLOCK);

    let mut first_fec_set: Vec<_> = data_shreds[..DATA_SHREDS_PER_FEC_BLOCK]
        .iter()
        .cloned()
        .chain(coding_shreds[..DATA_SHREDS_PER_FEC_BLOCK].iter().cloned())
        .collect();
    let chained_merkle_root = first_fec_set[0].chained_merkle_root().unwrap();

    for shred in first_fec_set.iter_mut().take(DATA_SHREDS_PER_FEC_BLOCK) {
        let mut payload = shred.payload().clone();
        payload.as_mut()[DATA_SHRED_FLAGS_OFFSET] |= ShredFlags::DATA_COMPLETE_SHRED.bits();
        *shred = Shred::new_from_serialized_shred(payload).unwrap();
    }
    finish_erasure_batch_for_tests(
        &leader_keypair,
        &mut first_fec_set,
        chained_merkle_root,
        &reed_solomon_cache,
    )
    .unwrap();

    let (mut data_shreds, mut coding_shreds): (Vec<_>, Vec<_>) =
        first_fec_set.into_iter().partition(Shred::is_data);
    data_shreds.sort_unstable_by_key(Shred::index);
    coding_shreds.sort_unstable_by_key(Shred::index);

    let valid_data_shred = data_shreds.pop().unwrap();
    let shreds: Vec<_> = std::iter::once(valid_data_shred.clone())
        .chain(
            coding_shreds
                .into_iter()
                .take(DATA_SHREDS_PER_FEC_BLOCK - 1),
        )
        .map(|shred| {
            (
                Cow::Owned(shred),
                /*is_repaired:*/ false,
                BlockLocation::Original,
            )
        })
        .collect();
    let (dummy_retransmit_sender, _) = EvictingSender::new_bounded(0);
    let mut metrics = BlockstoreInsertionMetrics::default();
    blockstore
        .do_insert_shreds(
            shreds,
            Some(&leader_schedule_cache),
            false, // is_trusted
            Some(&mut ShredRecoveryContext::new(
                reed_solomon_cache,
                dummy_retransmit_sender,
                root_bank,
                0, // shred_version
            )),
            &mut metrics,
        )
        .unwrap();

    assert_eq!(metrics.num_recovered, 0);
    assert_eq!(metrics.num_recovered_inserted, 0);
    assert_eq!(metrics.num_recovered_failed_invalid, 0);

    let index = blockstore.get_index(slot).unwrap().unwrap();
    assert_eq!(index.data().num_shreds(), 1);
    assert!(index.data().contains(u64::from(valid_data_shred.index())));
    assert_eq!(
        blockstore
            .get_data_shred(slot, u64::from(valid_data_shred.index()))
            .unwrap()
            .unwrap(),
        valid_data_shred.payload().as_ref(),
    );
    for shred in data_shreds {
        assert!(
            blockstore
                .get_data_shred(slot, u64::from(shred.index()))
                .unwrap()
                .is_none()
        );
    }
    verify_index_integrity(&blockstore, slot);
}

#[test]
fn test_index_integrity() {
    let slot = 1;
    let num_entries = 100;
    let (data_shreds, coding_shreds, leader_schedule_cache) =
        setup_erasure_shreds(slot, 0, num_entries);
    assert!(data_shreds.len() > 3);
    assert!(coding_shreds.len() > 3);

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Test inserting all the shreds
    let all_shreds: Vec<_> = data_shreds
        .iter()
        .cloned()
        .chain(coding_shreds.iter().cloned())
        .collect();
    blockstore
        .insert_shreds(all_shreds, Some(&leader_schedule_cache), false)
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();

    // Test inserting just the codes, enough for recovery
    blockstore
        .insert_shreds(coding_shreds.clone(), Some(&leader_schedule_cache), false)
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();

    // Test inserting some codes, but not enough for recovery
    blockstore
        .insert_shreds(
            coding_shreds[..coding_shreds.len() - 1].to_vec(),
            Some(&leader_schedule_cache),
            false,
        )
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();

    // Test inserting just the codes, and some data, enough for recovery
    let shreds: Vec<_> = data_shreds[..data_shreds.len() - 1]
        .iter()
        .cloned()
        .chain(coding_shreds[..coding_shreds.len() - 1].iter().cloned())
        .collect();
    blockstore
        .insert_shreds(shreds, Some(&leader_schedule_cache), false)
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();

    // Test inserting some codes, and some data, but enough for recovery
    let shreds: Vec<_> = data_shreds[..data_shreds.len() / 2 - 1]
        .iter()
        .cloned()
        .chain(coding_shreds[..coding_shreds.len() / 2 - 1].iter().cloned())
        .collect();
    blockstore
        .insert_shreds(shreds, Some(&leader_schedule_cache), false)
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();

    // Test inserting all shreds in 2 rounds, make sure nothing is lost
    let shreds1: Vec<_> = data_shreds[..data_shreds.len() / 2 - 1]
        .iter()
        .cloned()
        .chain(coding_shreds[..coding_shreds.len() / 2 - 1].iter().cloned())
        .collect();
    let shreds2: Vec<_> = data_shreds[data_shreds.len() / 2 - 1..]
        .iter()
        .cloned()
        .chain(coding_shreds[coding_shreds.len() / 2 - 1..].iter().cloned())
        .collect();
    blockstore
        .insert_shreds(shreds1, Some(&leader_schedule_cache), false)
        .unwrap();
    blockstore
        .insert_shreds(shreds2, Some(&leader_schedule_cache), false)
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();

    // Test not all, but enough data and coding shreds in 2 rounds to trigger recovery,
    // make sure nothing is lost
    let shreds1: Vec<_> = data_shreds[..data_shreds.len() / 2 - 1]
        .iter()
        .cloned()
        .chain(coding_shreds[..coding_shreds.len() / 2 - 1].iter().cloned())
        .collect();
    let shreds2: Vec<_> = data_shreds[data_shreds.len() / 2 - 1..data_shreds.len() / 2]
        .iter()
        .cloned()
        .chain(
            coding_shreds[coding_shreds.len() / 2 - 1..coding_shreds.len() / 2]
                .iter()
                .cloned(),
        )
        .collect();
    blockstore
        .insert_shreds(shreds1, Some(&leader_schedule_cache), false)
        .unwrap();
    blockstore
        .insert_shreds(shreds2, Some(&leader_schedule_cache), false)
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();

    // Test insert shreds in 2 rounds, but not enough to trigger
    // recovery, make sure nothing is lost
    let shreds1: Vec<_> = data_shreds[..data_shreds.len() / 2 - 2]
        .iter()
        .cloned()
        .chain(coding_shreds[..coding_shreds.len() / 2 - 2].iter().cloned())
        .collect();
    let shreds2: Vec<_> = data_shreds[data_shreds.len() / 2 - 2..data_shreds.len() / 2 - 1]
        .iter()
        .cloned()
        .chain(
            coding_shreds[coding_shreds.len() / 2 - 2..coding_shreds.len() / 2 - 1]
                .iter()
                .cloned(),
        )
        .collect();
    blockstore
        .insert_shreds(shreds1, Some(&leader_schedule_cache), false)
        .unwrap();
    blockstore
        .insert_shreds(shreds2, Some(&leader_schedule_cache), false)
        .unwrap();
    verify_index_integrity(&blockstore, slot);
    blockstore.purge_slots(0, slot, PurgeType::Exact).unwrap();
}

fn setup_erasure_shreds(
    slot: u64,
    parent_slot: u64,
    num_entries: u64,
) -> (Vec<Shred>, Vec<Shred>, Arc<LeaderScheduleCache>) {
    setup_erasure_shreds_with_index(slot, parent_slot, num_entries, 0)
}

fn setup_erasure_shreds_with_index(
    slot: u64,
    parent_slot: u64,
    num_entries: u64,
    fec_set_index: u32,
) -> (Vec<Shred>, Vec<Shred>, Arc<LeaderScheduleCache>) {
    setup_erasure_shreds_with_index_and_chained_merkle(
        slot,
        parent_slot,
        num_entries,
        fec_set_index,
        Hash::new_from_array(rand::rng().random()),
    )
}

fn setup_erasure_shreds_with_index_and_chained_merkle(
    slot: u64,
    parent_slot: u64,
    num_entries: u64,
    fec_set_index: u32,
    chained_merkle_root: Hash,
) -> (Vec<Shred>, Vec<Shred>, Arc<LeaderScheduleCache>) {
    setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot(
        slot,
        parent_slot,
        num_entries,
        fec_set_index,
        chained_merkle_root,
        true,
    )
}

fn setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot(
    slot: u64,
    parent_slot: u64,
    num_entries: u64,
    fec_set_index: u32,
    chained_merkle_root: Hash,
    is_last_in_slot: bool,
) -> (Vec<Shred>, Vec<Shred>, Arc<LeaderScheduleCache>) {
    let (data_shreds, coding_shreds, _, leader_schedule_cache) =
        setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot_and_keypair(
            slot,
            parent_slot,
            num_entries,
            fec_set_index,
            chained_merkle_root,
            is_last_in_slot,
        );

    (data_shreds, coding_shreds, leader_schedule_cache)
}

fn setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot_and_keypair(
    slot: u64,
    parent_slot: u64,
    num_entries: u64,
    fec_set_index: u32,
    chained_merkle_root: Hash,
    is_last_in_slot: bool,
) -> (
    Vec<Shred>,
    Vec<Shred>,
    Arc<Keypair>,
    Arc<LeaderScheduleCache>,
) {
    let entries = make_slot_entries_with_transactions(num_entries);
    let leader_keypair = Arc::new(Keypair::new());
    let shredder = Shredder::new(slot, parent_slot, 0, 0).unwrap();
    let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
        &leader_keypair,
        &entries,
        is_last_in_slot,
        chained_merkle_root,
        fec_set_index, // next_shred_index
        fec_set_index, // next_code_index
        &ReedSolomonCache::default(),
        &mut ProcessShredsStats::default(),
    );

    let genesis_config = create_genesis_config(2).genesis_config;
    let bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let mut leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
    let fixed_schedule = FixedSchedule {
        leader_schedule: Arc::new(LeaderSchedule::new_from_schedule(
            vec![SlotLeader {
                id: leader_keypair.pubkey(),
                vote_address: Pubkey::new_unique(),
            }],
            NonZeroUsize::new(1).unwrap(),
        )),
    };
    leader_schedule_cache.set_fixed_leader_schedule(Some(fixed_schedule));

    (
        data_shreds,
        coding_shreds,
        leader_keypair,
        Arc::new(leader_schedule_cache),
    )
}

fn verify_index_integrity(blockstore: &Blockstore, slot: u64) {
    let shred_index = blockstore.get_index(slot).unwrap().unwrap();

    let data_iter = blockstore.slot_data_iterator(slot, 0).unwrap();
    let mut num_data = 0;
    for ((slot, index), _) in data_iter {
        num_data += 1;
        // Test that iterator and individual shred lookup yield same set
        assert!(blockstore.get_data_shred(slot, index).unwrap().is_some());
        // Test that the data index has current shred accounted for
        assert!(shred_index.data().contains(index));
    }

    // Test the data index doesn't have anything extra
    let num_data_in_index = shred_index.data().num_shreds();
    assert_eq!(num_data_in_index, num_data);

    let coding_iter = blockstore.slot_coding_iterator(slot, 0).unwrap();
    let mut num_coding = 0;
    for ((slot, index), _) in coding_iter {
        num_coding += 1;
        // Test that the iterator and individual shred lookup yield same set
        assert!(blockstore.get_coding_shred(slot, index).unwrap().is_some());
        // Test that the coding index has current shred accounted for
        assert!(shred_index.coding().contains(index));
    }

    // Test the data index doesn't have anything extra
    let num_coding_in_index = shred_index.coding().num_shreds();
    assert_eq!(num_coding_in_index, num_coding);
}

#[test]
fn test_duplicate_slot() {
    let slot = 0;
    let entries1 = make_slot_entries_with_transactions(1);
    let entries2 = make_slot_entries_with_transactions(1);
    let leader_keypair = Arc::new(Keypair::new());
    let reed_solomon_cache = ReedSolomonCache::default();
    let shredder = Shredder::new(slot, 0, 0, 0).unwrap();
    let merkle_root = Hash::new_from_array(rand::rng().random());
    let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
        &leader_keypair,
        &entries1,
        true, // is_last_in_slot
        merkle_root,
        0, // next_shred_index
        0, // next_code_index,
        &reed_solomon_cache,
        &mut ProcessShredsStats::default(),
    );
    let (duplicate_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
        &leader_keypair,
        &entries2,
        true, // is_last_in_slot
        merkle_root,
        0, // next_shred_index
        0, // next_code_index
        &reed_solomon_cache,
        &mut ProcessShredsStats::default(),
    );
    let shred = shreds[0].clone();
    let duplicate_shred = duplicate_shreds[0].clone();
    let non_duplicate_shred = shred.clone();

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    blockstore
        .insert_shreds(vec![shred.clone()], None, false)
        .unwrap();

    // No duplicate shreds exist yet
    assert!(!blockstore.has_duplicate_shreds_in_slot(slot));

    // Check if shreds are duplicated
    assert_eq!(
        blockstore.is_shred_duplicate(&duplicate_shred).as_deref(),
        Some(shred.payload().as_ref())
    );
    assert!(
        blockstore
            .is_shred_duplicate(&non_duplicate_shred)
            .is_none()
    );

    // Store a duplicate shred
    blockstore
        .store_duplicate_slot(
            slot,
            shred.payload().clone(),
            duplicate_shred.payload().clone(),
        )
        .unwrap();

    // Slot is now marked as duplicate
    assert!(blockstore.has_duplicate_shreds_in_slot(slot));

    // Check ability to fetch the duplicates
    let duplicate_proof = blockstore.get_duplicate_slot(slot).unwrap();
    assert_eq!(duplicate_proof.shred1, *shred.payload());
    assert_eq!(duplicate_proof.shred2, *duplicate_shred.payload());
}

#[test]
fn test_clear_unconfirmed_slot() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let unconfirmed_slot = 9;
    let unconfirmed_child_slot = 10;
    let slots = vec![2, unconfirmed_slot, unconfirmed_child_slot];

    // Insert into slot 9, mark it as dead
    let shreds: Vec<_> = make_chaining_slot_entries(&slots, 1, 0)
        .into_iter()
        .flat_map(|x| x.0)
        .collect();
    blockstore.insert_shreds(shreds, None, false).unwrap();
    // There are 32 data shreds in slot 9.
    for index in 0..32 {
        assert_matches!(
            blockstore.get_data_shred(unconfirmed_slot, index as u64),
            Ok(Some(_))
        );
    }
    blockstore.set_dead_slot(unconfirmed_slot).unwrap();

    // Purge the slot
    blockstore.clear_unconfirmed_slot(unconfirmed_slot);
    assert!(!blockstore.is_dead(unconfirmed_slot));
    assert_eq!(
        blockstore
            .meta(unconfirmed_slot)
            .unwrap()
            .unwrap()
            .next_slots,
        vec![unconfirmed_child_slot]
    );
    assert!(
        blockstore
            .get_data_shred(unconfirmed_slot, 0)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_clear_unconfirmed_slot_and_insert_again() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let confirmed_slot = 7;
    let unconfirmed_slot = 8;
    let slots = vec![confirmed_slot, unconfirmed_slot];

    let shreds: Vec<_> = make_chaining_slot_entries(&slots, 1, 0)
        .into_iter()
        .flat_map(|x| x.0)
        .collect();
    assert_eq!(shreds.len(), 2 * 32);

    // Save off unconfirmed_slot for later, just one shred at shreds[32]
    let unconfirmed_slot_shreds = vec![shreds[32].clone()];
    assert_eq!(unconfirmed_slot_shreds[0].slot(), unconfirmed_slot);

    // Insert into slot 9
    blockstore.insert_shreds(shreds, None, false).unwrap();

    // Purge the slot
    blockstore.clear_unconfirmed_slot(unconfirmed_slot);
    assert!(!blockstore.is_dead(unconfirmed_slot));
    assert!(
        blockstore
            .get_data_shred(unconfirmed_slot, 0)
            .unwrap()
            .is_none()
    );

    // Re-add unconfirmed_slot and confirm that confirmed_slot only has
    // unconfirmed_slot in next_slots once
    blockstore
        .insert_shreds(unconfirmed_slot_shreds, None, false)
        .unwrap();
    assert_eq!(
        blockstore.meta(confirmed_slot).unwrap().unwrap().next_slots,
        vec![unconfirmed_slot]
    );
}

#[test]
fn test_update_completed_data_indexes() {
    let mut completed_data_indexes = CompletedDataIndexes::default();
    let mut shred_index = ShredIndex::default();

    for i in 0..10 {
        shred_index.insert(i as u64);
        assert!(
            update_completed_data_indexes(true, i, &shred_index, &mut completed_data_indexes)
                .eq(std::iter::once(i..i + 1))
        );
        assert!(completed_data_indexes.iter().eq(0..=i));
    }
}

#[test]
fn test_update_completed_data_indexes_out_of_order() {
    let mut completed_data_indexes = CompletedDataIndexes::default();
    let mut shred_index = ShredIndex::default();

    shred_index.insert(4);
    assert!(
        update_completed_data_indexes(false, 4, &shred_index, &mut completed_data_indexes).eq([])
    );
    assert!(completed_data_indexes.is_empty());

    shred_index.insert(2);
    assert!(
        update_completed_data_indexes(false, 2, &shred_index, &mut completed_data_indexes).eq([])
    );
    assert!(completed_data_indexes.is_empty());

    shred_index.insert(3);
    assert!(
        update_completed_data_indexes(true, 3, &shred_index, &mut completed_data_indexes).eq([])
    );
    assert!(completed_data_indexes.clone().iter().eq([3]));

    // Inserting data complete shred 1 now confirms the range of shreds [2, 3]
    // is part of the same data set
    shred_index.insert(1);
    assert!(
        update_completed_data_indexes(true, 1, &shred_index, &mut completed_data_indexes)
            .eq(std::iter::once(2..4))
    );
    assert!(completed_data_indexes.clone().iter().eq([1, 3]));

    // Inserting data complete shred 0 now confirms the range of shreds [0]
    // is part of the same data set
    shred_index.insert(0);
    assert!(
        update_completed_data_indexes(true, 0, &shred_index, &mut completed_data_indexes)
            .eq([0..1, 1..2])
    );
    assert!(completed_data_indexes.clone().iter().eq([0, 1, 3]));
}

#[test]
fn test_rewards_protobuf_backward_compatibility() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let rewards: Rewards = (0..100)
        .map(|i| Reward {
            pubkey: solana_pubkey::new_rand().to_string(),
            lamports: 42 + i,
            post_balance: u64::MAX,
            reward_type: Some(RewardType::Fee),
            commission: None,
            commission_bps: None,
        })
        .collect();
    let protobuf_rewards: generated::Rewards = rewards.into();

    let deprecated_rewards: StoredExtendedRewards = protobuf_rewards.clone().into();
    for slot in 0..2 {
        let data = bincode::serialize(&deprecated_rewards).unwrap();
        blockstore.rewards_cf.put_bytes(slot, &data).unwrap();
    }
    for slot in 2..4 {
        blockstore
            .rewards_cf
            .put_protobuf(slot, &protobuf_rewards)
            .unwrap();
    }
    for slot in 0..4 {
        assert_eq!(
            blockstore
                .rewards_cf
                .get_protobuf_or_wincode::<StoredExtendedRewards>(slot)
                .unwrap()
                .unwrap(),
            protobuf_rewards
        );
    }
}

// This test is probably superfluous, since it is highly unlikely that bincode-format
// TransactionStatus entries exist in any current ledger. They certainly exist in historical
// ledger archives, but typically those require contemporaraneous software for other reasons.
// However, we are persisting the test since the apis still exist in `blockstore_db`.
#[test]
fn test_transaction_status_protobuf_backward_compatibility() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let status = TransactionStatusMeta {
        status: Ok(()),
        fee: 42,
        pre_balances: vec![1, 2, 3],
        post_balances: vec![1, 2, 3],
        inner_instructions: Some(vec![]),
        log_messages: Some(vec![]),
        pre_token_balances: Some(vec![TransactionTokenBalance {
            account_index: 0,
            mint: Pubkey::new_unique().to_string(),
            ui_token_amount: UiTokenAmount {
                ui_amount: Some(1.1),
                decimals: 1,
                amount: "11".to_string(),
                ui_amount_string: "1.1".to_string(),
            },
            owner: Pubkey::new_unique().to_string(),
            program_id: Pubkey::new_unique().to_string(),
        }]),
        post_token_balances: Some(vec![TransactionTokenBalance {
            account_index: 0,
            mint: Pubkey::new_unique().to_string(),
            ui_token_amount: UiTokenAmount {
                ui_amount: None,
                decimals: 1,
                amount: "11".to_string(),
                ui_amount_string: "1.1".to_string(),
            },
            owner: Pubkey::new_unique().to_string(),
            program_id: Pubkey::new_unique().to_string(),
        }]),
        rewards: Some(vec![Reward {
            pubkey: "My11111111111111111111111111111111111111111".to_string(),
            lamports: -42,
            post_balance: 42,
            reward_type: Some(RewardType::Rent),
            commission: None,
            commission_bps: None,
        }]),
        loaded_addresses: LoadedAddresses::default(),
        return_data: Some(TransactionReturnData {
            program_id: Pubkey::new_unique(),
            data: vec![1, 2, 3],
        }),
        compute_units_consumed: Some(23456),
        cost_units: Some(5678),
    };
    let deprecated_status: StoredTransactionStatusMeta = status.clone().try_into().unwrap();
    let protobuf_status: generated::TransactionStatusMeta = status.into();

    for slot in 0..2 {
        let data = bincode::serialize(&deprecated_status).unwrap();
        blockstore
            .transaction_status_cf
            .put_bytes((Signature::default(), slot), &data)
            .unwrap();
    }
    for slot in 2..4 {
        blockstore
            .transaction_status_cf
            .put_protobuf((Signature::default(), slot), &protobuf_status)
            .unwrap();
    }
    for slot in 0..4 {
        assert_eq!(
            blockstore
                .transaction_status_cf
                .get_protobuf_or_wincode::<StoredTransactionStatusMeta>((
                    Signature::default(),
                    slot
                ))
                .unwrap()
                .unwrap(),
            protobuf_status
        );
    }
}

fn make_large_tx_entry(num_txs: usize) -> Entry {
    let txs: Vec<_> = (0..num_txs)
        .map(|_| {
            let keypair0 = Keypair::new();
            let to = solana_pubkey::new_rand();
            solana_system_transaction::transfer(&keypair0, &to, 1, Hash::default())
        })
        .collect();

    Entry::new(&Hash::default(), 1, txs)
}

#[test]
/// Intentionally makes the ErasureMeta mismatch via the code index
/// by producing valid shreds with overlapping fec_set_index ranges
/// so that we fail the config check and mark the slot duplicate
fn erasure_multiple_config() {
    agave_logger::setup();
    let slot = 1;
    let num_txs = 20;
    // primary slot content
    let entries = [make_large_tx_entry(num_txs)];
    // "conflicting" slot content
    let entries2 = [make_large_tx_entry(num_txs)];

    let version = version_from_hash(&entries[0].hash);
    let shredder = Shredder::new(slot, 0, 0, version).unwrap();
    let reed_solomon_cache = ReedSolomonCache::default();
    let merkle_root = Hash::new_from_array(rand::rng().random());
    let kp = Keypair::new();
    // produce normal shreds
    let (data1, coding1) = shredder.entries_to_merkle_shreds_for_tests(
        &kp,
        &entries,
        true, // complete slot
        merkle_root,
        0, // next_shred_index
        0, // next_code_index
        &reed_solomon_cache,
        &mut ProcessShredsStats::default(),
    );
    // produce shreds with conflicting FEC set index based off different data.
    // it should not matter what data we use here though. Using the same merkle
    // root as above as if we are building off the same previous block.
    let (_data2, coding2) = shredder.entries_to_merkle_shreds_for_tests(
        &kp,
        &entries2,
        true, // complete slot
        merkle_root,
        0, // next_shred_index
        1, // next_code_index (overlaps with FEC set in data1 + coding1)
        &reed_solomon_cache,
        &mut ProcessShredsStats::default(),
    );

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    for shred in &data1 {
        info!("shred {:?}", shred.id());
    }
    for shred in &coding1 {
        info!("coding1 {:?}", shred.id());
    }
    for shred in &coding2 {
        info!("coding2 {:?}", shred.id());
    }
    // insert all but 2 data shreds from first set (so it is not yet recoverable)
    blockstore
        .insert_shreds(data1[..data1.len() - 2].to_vec(), None, false)
        .unwrap();
    // insert two coding shreds from conflicting FEC sets
    blockstore
        .insert_shreds(vec![coding1[0].clone(), coding2[1].clone()], None, false)
        .unwrap();
    assert!(blockstore.has_duplicate_shreds_in_slot(slot));
}

#[test]
fn test_insert_data_shreds_same_slot_last_index() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Create enough entries to ensure there are at least two shreds created
    let num_unique_entries = max_ticks_per_n_shreds(1, None) + 1;
    let (mut original_shreds, original_entries) = make_slot_entries(0, 0, num_unique_entries);
    let mut duplicate_shreds = original_shreds.clone();
    // Mutate signature so that payloads are not the same as the originals.
    for shred in &mut duplicate_shreds {
        shred.sign(&Keypair::new());
    }
    // Discard first shred, so that the slot is not full
    assert!(original_shreds.len() > 1);
    let last_index = original_shreds.last().unwrap().index() as u64;
    original_shreds.remove(0);

    // Insert the same shreds, including the last shred specifically, multiple
    // times
    for _ in 0..10 {
        blockstore
            .insert_shreds(original_shreds.clone(), None, false)
            .unwrap();
        let meta = blockstore.meta(0).unwrap().unwrap();
        assert!(!blockstore.is_dead(0));
        assert_eq!(blockstore.get_slot_entries(0, 0).unwrap(), vec![]);
        assert_eq!(meta.consumed, 0);
        assert_eq!(meta.received, last_index + 1);
        assert_eq!(meta.parent_slot, Some(0));
        assert_eq!(meta.last_index, Some(last_index));
        assert!(!blockstore.is_full(0));
    }

    let num_shreds = duplicate_shreds.len() as u64;
    blockstore
        .insert_shreds(duplicate_shreds, None, false)
        .unwrap();

    assert_eq!(blockstore.get_slot_entries(0, 0).unwrap(), original_entries);

    let meta = blockstore.meta(0).unwrap().unwrap();
    assert_eq!(meta.consumed, num_shreds);
    assert_eq!(meta.received, num_shreds);
    assert_eq!(meta.parent_slot, Some(0));
    assert_eq!(meta.last_index, Some(num_shreds - 1));
    assert!(blockstore.is_full(0));
    assert!(!blockstore.is_dead(0));
}

/// Prepare two FEC sets of shreds for the same slot index
/// with reasonable shred indices, but in such a way that
/// both FEC sets include a shred with LAST_IN_SLOT flag set.
#[allow(clippy::type_complexity)]
fn setup_duplicate_last_in_slot(
    slot: Slot,
) -> ((Vec<Shred>, Vec<Shred>), (Vec<Shred>, Vec<Shred>)) {
    let entries = make_slot_entries_with_transactions(1);
    let leader_keypair = Arc::new(Keypair::new());
    let reed_solomon_cache = ReedSolomonCache::default();
    let shredder = Shredder::new(slot, 0, 0, 0).unwrap();
    let (shreds1, code1): (Vec<Shred>, Vec<Shred>) = shredder
        .make_merkle_shreds_from_entries(
            &leader_keypair,
            &entries,
            true,               // is_last_in_slot
            Hash::new_unique(), // chained_merkle_root
            0,                  // next_shred_index
            0,                  // next_code_index,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        )
        .partition(Shred::is_data);
    let last_data1 = shreds1.last().unwrap();
    let last_code1 = code1.last().unwrap();

    let (shreds2, code2) = shredder
        .make_merkle_shreds_from_entries(
            &leader_keypair,
            &entries,
            true, // is_last_in_slot
            last_data1.chained_merkle_root().unwrap(),
            last_data1.index() + 1, // next_shred_index
            last_code1.index() + 1, // next_code_index,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        )
        .partition(Shred::is_data);
    ((shreds1, code1), (shreds2, code2))
}

#[test]
fn test_duplicate_last_index() {
    let slot = 1;
    let ((shreds1, _code1), (shreds2, _code2)) = setup_duplicate_last_in_slot(slot);

    let last_data1 = shreds1.last().unwrap();
    let last_data2 = shreds2.last().unwrap();
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    blockstore
        .insert_shreds(vec![last_data1.clone(), last_data2.clone()], None, false)
        .unwrap();

    assert!(blockstore.get_duplicate_slot(slot).is_some());
}

#[test]
fn test_duplicate_last_index_mark_dead() {
    let num_shreds = 10;
    let smaller_last_shred_index = 31;
    let larger_last_shred_index = 8;

    let setup_test_shreds = |slot: Slot| -> Vec<Shred> {
        let ((mut shreds1, _code1), (mut shreds2, _code2)) = setup_duplicate_last_in_slot(slot);
        shreds1.append(&mut shreds2);
        shreds1
    };

    let get_expected_slot_meta_and_index_meta =
        |blockstore: &Blockstore, shreds: Vec<Shred>| -> (SlotMeta, Index) {
            let slot = shreds[0].slot();
            blockstore
                .insert_shreds(shreds.clone(), None, false)
                .unwrap();
            let meta = blockstore.meta(slot).unwrap().unwrap();
            assert_eq!(meta.consumed, shreds.len() as u64);
            let shreds_index = blockstore.get_index(slot).unwrap().unwrap();
            for i in 0..shreds.len() as u64 {
                assert!(shreds_index.data().contains(i));
            }

            // Cleanup the slot
            blockstore
                .purge_slots(slot, slot, PurgeType::Exact)
                .expect("Purge database operations failed");
            assert!(blockstore.meta(slot).unwrap().is_none());

            (meta, shreds_index)
        };

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let mut slot = 0;
    let shreds = setup_test_shreds(slot);

    // Case 1: Insert in the same batch. Since we're inserting the shreds in order,
    // any shreds > smaller_last_shred_index will not be inserted.
    let (expected_slot_meta, expected_index) = get_expected_slot_meta_and_index_meta(
        &blockstore,
        shreds[..=smaller_last_shred_index].to_vec(),
    );
    blockstore
        .insert_shreds(shreds.clone(), None, false)
        .unwrap();
    assert!(blockstore.get_duplicate_slot(slot).is_some());
    // Block is already full not marked dead
    assert!(!blockstore.is_dead(slot));
    for i in 0..num_shreds {
        if i <= smaller_last_shred_index as u64 {
            assert_eq!(
                blockstore.get_data_shred(slot, i).unwrap().unwrap(),
                shreds[i as usize].payload().as_ref(),
            );
        } else {
            assert!(blockstore.get_data_shred(slot, i).unwrap().is_none());
        }
    }
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.first_shred_timestamp = expected_slot_meta.first_shred_timestamp;
    assert_eq!(meta, expected_slot_meta);
    assert_eq!(blockstore.get_index(slot).unwrap().unwrap(), expected_index);

    // Case 3: Insert shreds in reverse so that consumed will not be updated. Now on insert, the
    // the slot should be marked as dead
    slot += 1;
    let mut shreds = setup_test_shreds(slot);
    shreds.reverse();
    blockstore
        .insert_shreds(shreds.clone(), None, false)
        .unwrap();
    assert!(blockstore.is_dead(slot));
    // All the shreds other than the two last index shreds because those two
    // are marked as last, but less than the first received index == 10.
    // The others will be inserted even after the slot is marked dead on attempted
    // insert of the first last_index shred since dead slots can still be
    // inserted into.
    for i in 0..num_shreds {
        let shred_to_check = &shreds[i as usize];
        let shred_index = shred_to_check.index() as u64;
        if shred_index != smaller_last_shred_index as u64
            && shred_index != larger_last_shred_index as u64
        {
            assert_eq!(
                blockstore
                    .get_data_shred(slot, shred_index)
                    .unwrap()
                    .unwrap(),
                shred_to_check.payload().as_ref(),
            );
        } else {
            assert!(
                blockstore
                    .get_data_shred(slot, shred_index)
                    .unwrap()
                    .is_none()
            );
        }
    }

    // Case 4: Same as Case 3, but this time insert the shreds one at a time to test that the clearing
    // of data shreds works even after they've been committed
    slot += 1;
    let mut shreds = setup_test_shreds(slot);
    shreds.reverse();
    for shred in shreds.clone() {
        blockstore.insert_shreds(vec![shred], None, false).unwrap();
    }
    assert!(blockstore.is_dead(slot));
    // All the shreds will be inserted since dead slots can still be inserted into.
    for i in 0..num_shreds {
        let shred_to_check = &shreds[i as usize];
        let shred_index = shred_to_check.index() as u64;
        if shred_index != smaller_last_shred_index as u64
            && shred_index != larger_last_shred_index as u64
        {
            assert_eq!(
                blockstore
                    .get_data_shred(slot, shred_index)
                    .unwrap()
                    .unwrap(),
                shred_to_check.payload().as_ref(),
            );
        } else {
            assert!(
                blockstore
                    .get_data_shred(slot, shred_index)
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn test_get_slot_entries_dead_slot_race() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    {
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (slot_sender, slot_receiver) = unbounded();
        let (shred_sender, shred_receiver) = unbounded::<Vec<Shred>>();
        let (signal_sender, signal_receiver) = unbounded();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                while let Ok(slot) = slot_receiver.recv() {
                    match blockstore.get_slot_entries_with_shred_info(slot, 0, false) {
                        Ok((_entries, _num_shreds, is_full)) => {
                            if is_full {
                                signal_sender
                                    .send(Err(IoError::other(
                                        "got full slot entries for dead slot",
                                    )))
                                    .unwrap();
                            }
                        }
                        Err(err) => {
                            assert_matches!(err, BlockstoreError::DeadSlot);
                        }
                    }
                    signal_sender.send(Ok(())).unwrap();
                }
            });

            scope.spawn(|| {
                while let Ok(shreds) = shred_receiver.recv() {
                    let slot = shreds[0].slot();
                    // Grab this lock to block `get_slot_entries` before it fetches completed datasets
                    // and then mark the slot as dead, but full, by inserting carefully crafted shreds.

                    #[allow(clippy::readonly_write_lock)]
                    // Possible clippy bug, the lock is unused so clippy shouldn't care
                    // about read vs. write lock
                    let _lowest_cleanup_slot = blockstore.lowest_cleanup_slot.write().unwrap();
                    blockstore.insert_shreds(shreds, None, false).unwrap();
                    assert!(blockstore.get_duplicate_slot(slot).is_some());
                    assert!(blockstore.is_dead(slot));
                    signal_sender.send(Ok(())).unwrap();
                }
            });

            for slot in 0..100 {
                let ((mut shreds1, _), (mut shreds2, _)) = setup_duplicate_last_in_slot(slot);
                // compose shreds in reverse order of FEC sets to
                // make sure slot is marked dead
                shreds2.append(&mut shreds1);
                // Start a task on each thread to trigger a race condition
                slot_sender.send(slot).unwrap();
                shred_sender.send(shreds2).unwrap();

                // Check that each thread processed their task before continuing
                for _ in 1..=2 {
                    let res = signal_receiver.recv().unwrap();
                    assert!(res.is_ok(), "race condition: {res:?}");
                }
            }

            drop(slot_sender);
            drop(shred_sender);
        });
    }
}

#[test]
fn test_previous_erasure_set() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let mut erasure_metas = BTreeMap::new();

    let parent_slot = 0;
    let prev_slot = 1;
    let slot = 2;
    let (data_shreds_0, coding_shreds_0, _) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, 0);
    let erasure_set_0 = ErasureSetId::new(slot, 0);
    let erasure_meta_0 = ErasureMeta::from_coding_shred(coding_shreds_0.first().unwrap()).unwrap();

    let prev_fec_set_index = data_shreds_0.len() as u32;
    let (data_shreds_prev, coding_shreds_prev, _) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, prev_fec_set_index);
    let erasure_set_prev = ErasureSetId::new(slot, prev_fec_set_index);
    let erasure_meta_prev =
        ErasureMeta::from_coding_shred(coding_shreds_prev.first().unwrap()).unwrap();

    let (_, coding_shreds_prev_slot, _) =
        setup_erasure_shreds_with_index(prev_slot, parent_slot, 10, prev_fec_set_index);
    let erasure_set_prev_slot = ErasureSetId::new(prev_slot, prev_fec_set_index);
    let erasure_meta_prev_slot =
        ErasureMeta::from_coding_shred(coding_shreds_prev_slot.first().unwrap()).unwrap();

    let fec_set_index = data_shreds_prev.len() as u32 + prev_fec_set_index;
    let erasure_set = ErasureSetId::new(slot, fec_set_index);

    // Blockstore is empty
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap(),
        None
    );

    // Erasure metas does not contain the previous fec set, but only the one before that
    erasure_metas.insert(erasure_set_0, WorkingEntry::Dirty(erasure_meta_0));
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap(),
        None
    );

    // Both Erasure metas and blockstore, contain only contain the previous previous fec set
    erasure_metas.insert(erasure_set_0, WorkingEntry::Clean(erasure_meta_0));
    blockstore
        .put_erasure_meta(erasure_set_0, &erasure_meta_0)
        .unwrap();
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap(),
        None
    );

    // Erasure meta contains the previous FEC set, blockstore only contains the older
    erasure_metas.insert(erasure_set_prev, WorkingEntry::Dirty(erasure_meta_prev));
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap()
            .map(|(erasure_set, erasure_meta)| (erasure_set, erasure_meta.into_owned())),
        Some((erasure_set_prev, erasure_meta_prev))
    );

    // Erasure meta only contains the older, blockstore has the previous fec set
    erasure_metas.remove(&erasure_set_prev);
    blockstore
        .put_erasure_meta(erasure_set_prev, &erasure_meta_prev)
        .unwrap();
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap()
            .map(|(erasure_set, erasure_meta)| (erasure_set, erasure_meta.into_owned())),
        Some((erasure_set_prev, erasure_meta_prev))
    );

    // Both contain the previous fec set
    erasure_metas.insert(erasure_set_prev, WorkingEntry::Clean(erasure_meta_prev));
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap()
            .map(|(erasure_set, erasure_meta)| (erasure_set, erasure_meta.into_owned())),
        Some((erasure_set_prev, erasure_meta_prev))
    );

    // Works even if the previous fec set has index 0
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set_prev, &erasure_metas)
            .unwrap()
            .map(|(erasure_set, erasure_meta)| (erasure_set, erasure_meta.into_owned())),
        Some((erasure_set_0, erasure_meta_0))
    );
    erasure_metas.remove(&erasure_set_0);
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set_prev, &erasure_metas)
            .unwrap()
            .map(|(erasure_set, erasure_meta)| (erasure_set, erasure_meta.into_owned())),
        Some((erasure_set_0, erasure_meta_0))
    );

    // Does not cross slot boundary
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    erasure_metas.clear();
    erasure_metas.insert(
        erasure_set_prev_slot,
        WorkingEntry::Dirty(erasure_meta_prev_slot),
    );
    assert_eq!(
        erasure_meta_prev_slot.next_fec_set_index().unwrap(),
        fec_set_index
    );
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap(),
        None,
    );
    erasure_metas.insert(
        erasure_set_prev_slot,
        WorkingEntry::Clean(erasure_meta_prev_slot),
    );
    blockstore
        .put_erasure_meta(erasure_set_prev_slot, &erasure_meta_prev_slot)
        .unwrap();
    assert_eq!(
        blockstore
            .previous_erasure_set(erasure_set, &erasure_metas)
            .unwrap(),
        None,
    );
}

#[test]
fn test_chained_merkle_root_consistency_backwards() {
    // Insert a coding shred then consistent data and coding shreds from the next FEC set
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred = coding_shreds[0].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred.clone(), &leader_schedule,)
            .is_empty()
    );

    let merkle_root = coding_shred.merkle_root().unwrap();

    // Correctly chained merkle
    let (data_shreds, coding_shreds, _) = setup_erasure_shreds_with_index_and_chained_merkle(
        slot,
        parent_slot,
        10,
        next_fec_set_index,
        merkle_root,
    );
    let data_shred = data_shreds[0].clone();
    let coding_shred = coding_shreds[0].clone();
    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred, &leader_schedule,)
            .is_empty()
    );
    assert!(
        blockstore
            .insert_shred_return_duplicate(data_shred, &leader_schedule,)
            .is_empty()
    );
}

#[test]
fn test_chained_merkle_root_consistency_forwards() {
    // Insert a coding shred, then a consistent coding shred from the previous FEC set
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred = coding_shreds[0].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    // Correctly chained merkle
    let merkle_root = coding_shred.merkle_root().unwrap();
    let (_, next_coding_shreds, _) = setup_erasure_shreds_with_index_and_chained_merkle(
        slot,
        parent_slot,
        10,
        next_fec_set_index,
        merkle_root,
    );
    let next_coding_shred = next_coding_shreds[0].clone();

    assert!(
        blockstore
            .insert_shred_return_duplicate(next_coding_shred, &leader_schedule,)
            .is_empty()
    );

    // Insert previous FEC set
    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred, &leader_schedule,)
            .is_empty()
    );
}

#[test]
fn test_chained_merkle_root_across_slots_backwards() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, _, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let data_shred = data_shreds[0].clone();

    assert!(
        blockstore
            .insert_shred_return_duplicate(data_shred.clone(), &leader_schedule,)
            .is_empty()
    );

    // Incorrectly chained merkle for next slot
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != data_shred.merkle_root().unwrap());
    let (next_slot_data_shreds, next_slot_coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot + 1,
            slot,
            10,
            fec_set_index,
            merkle_root,
        );
    let next_slot_data_shred = next_slot_data_shreds[0].clone();
    let next_slot_coding_shred = next_slot_coding_shreds[0].clone();
    assert!(
        blockstore
            .insert_shred_return_duplicate(next_slot_coding_shred, &leader_schedule,)
            .is_empty()
    );
    assert!(
        blockstore
            .insert_shred_return_duplicate(next_slot_data_shred, &leader_schedule)
            .is_empty()
    );
}

#[test]
fn test_chained_merkle_root_across_slots_forwards() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (_, coding_shreds, _) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred = coding_shreds[0].clone();

    // Incorrectly chained merkle for next slot
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != coding_shred.merkle_root().unwrap());
    let (next_slot_data_shreds, _, leader_schedule) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot + 1,
            slot,
            10,
            fec_set_index,
            merkle_root,
        );
    let next_slot_data_shred = next_slot_data_shreds[0].clone();

    assert!(
        blockstore
            .insert_shred_return_duplicate(next_slot_data_shred, &leader_schedule,)
            .is_empty()
    );

    // Insert for previous slot
    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred, &leader_schedule,)
            .is_empty()
    );
}

#[test]
fn test_chained_merkle_root_inconsistency_backwards_insert_code() {
    // Insert a coding shred then inconsistent coding shred then inconsistent data shred from the next FEC set
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred_previous = coding_shreds[0].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred_previous.clone(), &leader_schedule,)
            .is_empty()
    );

    // Incorrectly chained merkle
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != coding_shred_previous.merkle_root().unwrap());
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot,
            parent_slot,
            10,
            next_fec_set_index,
            merkle_root,
        );
    let data_shred = data_shreds[0].clone();
    let coding_shred = coding_shreds[0].clone();
    let duplicate_shreds =
        blockstore.insert_shred_return_duplicate(coding_shred.clone(), &leader_schedule);
    assert_eq!(duplicate_shreds.len(), 1);
    assert_eq!(
        duplicate_shreds[0],
        PossibleDuplicateShred::ChainedMerkleRootConflict(coding_shred.slot())
    );

    // Should not check again, even though this shred conflicts as well
    assert!(
        blockstore
            .insert_shred_return_duplicate(data_shred, &leader_schedule,)
            .is_empty()
    );
}

#[test]
fn test_chained_merkle_root_inconsistency_backwards_insert_data() {
    // Insert a coding shred then inconsistent data shred then inconsistent coding shred from the next FEC set
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred_previous = coding_shreds[0].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred_previous.clone(), &leader_schedule,)
            .is_empty()
    );

    // Incorrectly chained merkle
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != coding_shred_previous.merkle_root().unwrap());
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot,
            parent_slot,
            10,
            next_fec_set_index,
            merkle_root,
        );
    let data_shred = data_shreds[0].clone();
    let coding_shred = coding_shreds[0].clone();

    let duplicate_shreds =
        blockstore.insert_shred_return_duplicate(data_shred.clone(), &leader_schedule);
    assert_eq!(duplicate_shreds.len(), 1);
    assert_eq!(
        duplicate_shreds[0],
        PossibleDuplicateShred::ChainedMerkleRootConflict(data_shred.slot())
    );
    // Should not check again, even though this shred conflicts as well
    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred, &leader_schedule,)
            .is_empty()
    );
}

#[test]
fn test_chained_merkle_root_inconsistency_forwards() {
    // Insert a data shred, then an inconsistent coding shred from the previous FEC set
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred = coding_shreds[0].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    // Incorrectly chained merkle
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != coding_shred.merkle_root().unwrap());
    let (next_data_shreds, _, leader_schedule_next) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot,
            parent_slot,
            10,
            next_fec_set_index,
            merkle_root,
        );
    let next_data_shred = next_data_shreds[0].clone();

    assert!(
        blockstore
            .insert_shred_return_duplicate(next_data_shred.clone(), &leader_schedule_next,)
            .is_empty()
    );

    // Insert previous FEC set
    let duplicate_shreds =
        blockstore.insert_shred_return_duplicate(coding_shred.clone(), &leader_schedule);

    assert_eq!(duplicate_shreds.len(), 2);
    assert!(
        duplicate_shreds.contains(&PossibleDuplicateShred::ChainedMerkleRootConflict(
            coding_shred.slot(),
        ))
    );
    assert!(
        duplicate_shreds.contains(&PossibleDuplicateShred::FixedFECChainedMerkleRootConflict(
            coding_shred.slot(),
        ))
    );
}

#[test]
fn test_chained_merkle_root_inconsistency_data_shreds_only() {
    // Insert data shreds from consecutive FEC sets without any coding shreds.
    // The ErasureMeta-based SIMD-0340 check cannot run, so the fixed-FEC
    // MerkleRootMeta check must catch the inconsistent chain.
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, _, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let data_shred_previous = data_shreds[0].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    assert!(
        blockstore
            .insert_shred_return_duplicate(data_shred_previous.clone(), &leader_schedule,)
            .is_empty()
    );

    let merkle_root = Hash::new_unique();
    assert!(merkle_root != data_shred_previous.merkle_root().unwrap());
    let (data_shreds, _, leader_schedule) = setup_erasure_shreds_with_index_and_chained_merkle(
        slot,
        parent_slot,
        10,
        next_fec_set_index,
        merkle_root,
    );
    let data_shred = data_shreds[0].clone();

    let duplicate_shreds =
        blockstore.insert_shred_return_duplicate(data_shred.clone(), &leader_schedule);
    assert_eq!(duplicate_shreds.len(), 1);
    assert_eq!(
        duplicate_shreds[0],
        PossibleDuplicateShred::FixedFECChainedMerkleRootConflict(data_shred.slot())
    );
}

#[test]
fn test_chained_merkle_root_inconsistency_both() {
    // Insert a coding shred from fec_set - 1, and a data shred from fec_set + 10
    // Then insert an inconsistent data shred from fec_set, and finally an
    // inconsistent coding shred from fec_set
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let prev_fec_set_index = 0;
    let (prev_data_shreds, prev_coding_shreds, leader_schedule_prev) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, prev_fec_set_index);
    let prev_coding_shred = prev_coding_shreds[0].clone();
    let fec_set_index = prev_fec_set_index + prev_data_shreds.len() as u32;

    // Incorrectly chained merkle
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != prev_coding_shred.merkle_root().unwrap());
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot,
            parent_slot,
            10,
            fec_set_index,
            merkle_root,
        );
    let data_shred = data_shreds[0].clone();
    let coding_shred = coding_shreds[0].clone();
    let next_fec_set_index = fec_set_index + prev_data_shreds.len() as u32;

    // Incorrectly chained merkle
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != data_shred.merkle_root().unwrap());
    let (next_data_shreds, _, leader_schedule_next) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot,
            parent_slot,
            10,
            next_fec_set_index,
            merkle_root,
        );
    let next_data_shred = next_data_shreds[0].clone();

    assert!(
        blockstore
            .insert_shred_return_duplicate(prev_coding_shred.clone(), &leader_schedule_prev,)
            .is_empty()
    );

    assert!(
        blockstore
            .insert_shred_return_duplicate(next_data_shred.clone(), &leader_schedule_next)
            .is_empty()
    );

    // Insert data shred
    let duplicate_shreds =
        blockstore.insert_shred_return_duplicate(data_shred.clone(), &leader_schedule);

    // Only the backwards check will be performed
    assert_eq!(duplicate_shreds.len(), 1);
    assert_eq!(
        duplicate_shreds[0],
        PossibleDuplicateShred::ChainedMerkleRootConflict(data_shred.slot())
    );

    // Insert coding shred
    let duplicate_shreds =
        blockstore.insert_shred_return_duplicate(coding_shred.clone(), &leader_schedule);

    // Now the forwards check will be performed
    assert_eq!(duplicate_shreds.len(), 1);
    assert_eq!(
        duplicate_shreds[0],
        PossibleDuplicateShred::ChainedMerkleRootConflict(coding_shred.slot())
    );
}

#[test]
fn test_chained_merkle_root_upgrade_inconsistency_backwards() {
    // Insert a coding shred (with an old erasure meta and no merkle root meta) then inconsistent shreds from the next FEC set
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred_previous = coding_shreds[1].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred_previous.clone(), &leader_schedule,)
            .is_empty()
    );

    // Set the first received coding shred index to 0 and remove merkle root meta to simulate this insertion coming from an
    // older version.
    let mut erasure_meta = blockstore
        .erasure_meta(coding_shred_previous.erasure_set())
        .unwrap()
        .unwrap();
    erasure_meta.clear_first_received_coding_shred_index();
    blockstore
        .put_erasure_meta(coding_shred_previous.erasure_set(), &erasure_meta)
        .unwrap();
    let mut write_batch = blockstore.get_write_batch().unwrap();
    blockstore
        .merkle_root_meta_cf
        .delete_range_in_batch(&mut write_batch, slot, slot);
    blockstore.write_batch(write_batch).unwrap();
    assert!(
        blockstore
            .merkle_root_meta(coding_shred_previous.erasure_set())
            .unwrap()
            .is_none()
    );

    // Add an incorrectly chained merkle from the next set. Although incorrectly chained
    // we skip the duplicate check as the first received coding shred index shred is missing
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != coding_shred_previous.merkle_root().unwrap());
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot,
            parent_slot,
            10,
            next_fec_set_index,
            merkle_root,
        );
    let data_shred = data_shreds[0].clone();
    let coding_shred = coding_shreds[0].clone();
    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred, &leader_schedule)
            .is_empty()
    );
    assert!(
        blockstore
            .insert_shred_return_duplicate(data_shred, &leader_schedule,)
            .is_empty()
    );
}

#[test]
fn test_chained_merkle_root_upgrade_inconsistency_forwards() {
    // Insert a data shred (without a merkle root), then an inconsistent coding shred from the previous FEC set.
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 0;
    let slot = 1;
    let fec_set_index = 0;
    let (data_shreds, coding_shreds, leader_schedule) =
        setup_erasure_shreds_with_index(slot, parent_slot, 10, fec_set_index);
    let coding_shred = coding_shreds[0].clone();
    let next_fec_set_index = fec_set_index + data_shreds.len() as u32;

    // Incorrectly chained merkle
    let merkle_root = Hash::new_unique();
    assert!(merkle_root != coding_shred.merkle_root().unwrap());
    let (next_data_shreds, next_coding_shreds, leader_schedule_next) =
        setup_erasure_shreds_with_index_and_chained_merkle(
            slot,
            parent_slot,
            10,
            next_fec_set_index,
            merkle_root,
        );
    let next_data_shred = next_data_shreds[0].clone();

    assert!(
        blockstore
            .insert_shred_return_duplicate(next_data_shred, &leader_schedule_next,)
            .is_empty()
    );

    // Remove the merkle root meta in order to simulate this blockstore originating from
    // an older version.
    let mut write_batch = blockstore.get_write_batch().unwrap();
    blockstore
        .merkle_root_meta_cf
        .delete_range_in_batch(&mut write_batch, slot, slot);
    blockstore.write_batch(write_batch).unwrap();
    assert!(
        blockstore
            .merkle_root_meta(next_coding_shreds[0].erasure_set())
            .unwrap()
            .is_none()
    );

    // Insert previous FEC set, although incorrectly chained we skip the duplicate check
    // as the merkle root meta is missing.
    assert!(
        blockstore
            .insert_shred_return_duplicate(coding_shred, &leader_schedule)
            .is_empty()
    );
}

#[test]
fn test_write_transaction_memos() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let signature: Signature = Signature::new_unique();

    blockstore
        .write_transaction_memos(&signature, 4, "test_write_transaction_memos".to_string())
        .unwrap();

    let memo = blockstore
        .read_transaction_memos(signature, 4)
        .expect("Expected to find memo");
    assert_eq!(memo, Some("test_write_transaction_memos".to_string()));
}

#[test]
fn test_add_transaction_memos_to_batch() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let signatures: Vec<Signature> = (0..2).map(|_| Signature::new_unique()).collect();
    let mut memos_batch = blockstore.get_write_batch().unwrap();

    blockstore
        .add_transaction_memos_to_batch(
            &signatures[0],
            4,
            "test_write_transaction_memos1".to_string(),
            &mut memos_batch,
        )
        .unwrap();

    blockstore
        .add_transaction_memos_to_batch(
            &signatures[1],
            5,
            "test_write_transaction_memos2".to_string(),
            &mut memos_batch,
        )
        .unwrap();

    blockstore.write_batch(memos_batch).unwrap();

    let memo1 = blockstore
        .read_transaction_memos(signatures[0], 4)
        .expect("Expected to find memo");
    assert_eq!(memo1, Some("test_write_transaction_memos1".to_string()));

    let memo2 = blockstore
        .read_transaction_memos(signatures[1], 5)
        .expect("Expected to find memo");
    assert_eq!(memo2, Some("test_write_transaction_memos2".to_string()));
}

#[test]
fn test_write_transaction_status() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let signatures: Vec<Signature> = (0..2).map(|_| Signature::new_unique()).collect();
    let keys_with_writable: Vec<(Pubkey, bool)> =
        vec![(Pubkey::new_unique(), true), (Pubkey::new_unique(), false)];
    let slot = 5;

    blockstore
        .write_transaction_status(
            slot,
            signatures[0],
            keys_with_writable
                .iter()
                .map(|&(ref pubkey, writable)| (pubkey, writable)),
            TransactionStatusMeta {
                fee: 4200,
                ..TransactionStatusMeta::default()
            },
            0,
        )
        .unwrap();

    let tx_status = blockstore
        .read_transaction_status((signatures[0], slot))
        .unwrap()
        .unwrap();
    assert_eq!(tx_status.fee, 4200);
}

#[test]
fn test_add_transaction_status_to_batch() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let signatures: Vec<Signature> = (0..2).map(|_| Signature::new_unique()).collect();
    let keys_with_writable: Vec<Vec<(Pubkey, bool)>> = (0..2)
        .map(|_| vec![(Pubkey::new_unique(), true), (Pubkey::new_unique(), false)])
        .collect();
    let slot = 5;
    let mut status_batch = blockstore.get_write_batch().unwrap();

    for (tx_idx, signature) in signatures.iter().enumerate() {
        blockstore
            .add_transaction_status_to_batch(
                slot,
                *signature,
                keys_with_writable[tx_idx].iter().map(|(k, v)| (k, *v)),
                TransactionStatusMeta {
                    fee: 5700 + tx_idx as u64,
                    status: if tx_idx % 2 == 0 {
                        Ok(())
                    } else {
                        Err(TransactionError::InsufficientFundsForFee)
                    },
                    ..TransactionStatusMeta::default()
                },
                tx_idx,
                &mut status_batch,
            )
            .unwrap();
    }

    blockstore.write_batch(status_batch).unwrap();

    let tx_status1 = blockstore
        .read_transaction_status((signatures[0], slot))
        .unwrap()
        .unwrap();
    assert_eq!(tx_status1.fee, 5700);
    assert_eq!(tx_status1.status, Ok(()));

    let tx_status2 = blockstore
        .read_transaction_status((signatures[1], slot))
        .unwrap()
        .unwrap();
    assert_eq!(tx_status2.fee, 5701);
    assert_eq!(
        tx_status2.status,
        Err(TransactionError::InsufficientFundsForFee)
    );
}

#[test_case(false ; "original_location")]
#[test_case(true ; "alternate_location")]
fn test_get_double_merkle_root(use_alternate_location: bool) {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_slot = 990;
    let parent_block_id = Hash::default();
    let slot = 1000;
    let num_entries = 200;

    // Create a set of shreds for a complete block
    let (data_shreds, _, leader_schedule) = setup_erasure_shreds(slot, parent_slot, num_entries);

    // Collect FEC set merkle roots for verification
    let mut fec_set_roots = [Hash::default(); 3];
    for shred in data_shreds.iter() {
        if shred.index() % (DATA_SHREDS_PER_FEC_BLOCK as u32) == 0 {
            fec_set_roots[(shred.index() as usize) / DATA_SHREDS_PER_FEC_BLOCK] =
                shred.merkle_root().unwrap();
        }
    }

    let fec_set_count = u32::try_from(fec_set_roots.len()).unwrap();
    let parent_info_hash = hashv(&[
        &parent_slot.to_le_bytes(),
        parent_block_id.as_ref(),
        &fec_set_count.to_le_bytes(),
    ]);
    let merkle_tree_leaves: Vec<_> = fec_set_roots
        .iter()
        .copied()
        .chain(std::iter::once(parent_info_hash))
        .map(Ok)
        .collect();
    let merkle_tree = MerkleTree::try_new(merkle_tree_leaves.into_iter()).unwrap();
    let expected_double_merkle_root = *merkle_tree.root();

    let block_location = if use_alternate_location {
        BlockLocation::Alternate {
            block_id: expected_double_merkle_root,
        }
    } else {
        BlockLocation::Original
    };

    // Insert shreds into blockstore at the specified location
    let shreds = data_shreds
        .iter()
        .map(|shred| (Cow::Borrowed(shred), use_alternate_location, block_location));
    let insert_results = blockstore
        .do_insert_shreds(
            shreds,
            Some(&leader_schedule),
            false,
            None,
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();
    assert!(insert_results.duplicate_shreds.is_empty());

    let slot_meta = blockstore
        .meta_from_location(slot, block_location)
        .unwrap()
        .unwrap();
    assert!(slot_meta.is_full());

    // Test getting the double merkle root
    let double_merkle_root = blockstore
        .get_double_merkle_root(slot, block_location)
        .unwrap()
        .unwrap();

    let double_merkle_meta = blockstore
        .double_merkle_meta_cf
        .get((slot, block_location))
        .unwrap()
        .unwrap();

    // Verify the double merkle root matches our pre-computed value
    assert_eq!(double_merkle_root, expected_double_merkle_root);
    assert_eq!(double_merkle_meta.double_merkle_root, double_merkle_root);
    assert_eq!(double_merkle_meta.fec_set_count, 3); // With 200 entries, we should have 3 FEC sets
    // Proofs are empty
    assert_eq!(double_merkle_meta.proofs.len(), 0);

    // Generate the proofs
    let double_merkle_meta = blockstore
        .get_double_merkle_meta_maybe_populate_proofs(slot, block_location)
        .unwrap()
        .unwrap();
    let proof_size = get_proof_size(double_merkle_meta.fec_set_count as usize + 1) as usize;
    assert_eq!(
        double_merkle_meta.proofs.len(),
        4 * proof_size * SIZE_OF_MERKLE_PROOF_ENTRY
    ); // 3 FEC sets + 1 parent info

    // Verify the proofs
    // FEC sets
    for (fec_set, root) in fec_set_roots.iter().enumerate() {
        verify_merkle_proof(
            *root,
            fec_set,
            double_merkle_meta
                .get_fec_set_proof(fec_set as u32)
                .unwrap(),
            double_merkle_meta.double_merkle_root,
        )
        .unwrap();
    }

    // Parent info - final proof
    verify_merkle_proof(
        parent_info_hash,
        double_merkle_meta.fec_set_count as usize,
        double_merkle_meta.get_parent_info_proof().unwrap(),
        double_merkle_meta.double_merkle_root,
    )
    .unwrap();

    // Slot not full should return None
    let incomplete_slot = 1001;
    let (partial_shreds, _, leader_schedule) =
        setup_erasure_shreds_with_index_and_chained_merkle_and_last_in_slot(
            incomplete_slot,
            slot, // parent is 1000
            5,
            0,
            Hash::new_from_array(rand::random()),
            false, // not last in slot
        );

    let shreds = partial_shreds
        .iter()
        .take(3)
        .map(|shred| (Cow::Borrowed(shred), use_alternate_location, block_location));
    let insert_results = blockstore
        .do_insert_shreds(
            shreds,
            Some(&leader_schedule),
            false,
            None,
            &mut BlockstoreInsertionMetrics::default(),
        )
        .unwrap();
    assert!(insert_results.duplicate_shreds.is_empty());

    assert!(
        blockstore
            .get_double_merkle_root(incomplete_slot, block_location)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_get_data_shreds_for_slot() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let parent_slot = 990;
    let slot = 1000;
    let num_entries = 200;
    let locations = [
        BlockLocation::Original,
        BlockLocation::Alternate {
            block_id: Hash::new_from_array([1u8; HASH_BYTES]),
        },
        BlockLocation::Alternate {
            block_id: Hash::new_from_array([2u8; HASH_BYTES]),
        },
        BlockLocation::Alternate {
            block_id: Hash::new_from_array([3u8; HASH_BYTES]),
        },
    ];

    // Setup and insert shreds from 4 blocks into the columns for `slot`
    let data_shreds: [Vec<Shred>; 4] = std::array::from_fn(|i| {
        let (data_shreds, _coding_shreds, leader_schedule_cache) =
            setup_erasure_shreds(slot, parent_slot, num_entries);
        let location = locations[i];
        let is_repaired = location != BlockLocation::Original;
        let shreds = data_shreds
            .iter()
            .map(|shred| (Cow::Borrowed(shred), is_repaired, location));
        let insert_results = blockstore
            .do_insert_shreds(
                shreds,
                Some(&leader_schedule_cache),
                false,
                None,
                &mut BlockstoreInsertionMetrics::default(),
            )
            .unwrap();
        assert!(insert_results.duplicate_shreds.is_empty());
        data_shreds
    });

    // Verify that each block is able to be fetched
    for (i, location) in locations.iter().enumerate() {
        let shreds = &data_shreds[i];
        let len = shreds.len();
        let start_indices = [0, len / 2, len - 1];
        for start_index in start_indices {
            let expected_shreds = &shreds[start_index..];
            let fetched_shreds = blockstore
                .get_data_shreds_for_slot_from_location(slot, start_index as u64, *location)
                .unwrap();
            assert_eq!(fetched_shreds.len(), expected_shreds.len());
            for (fetched, expected) in fetched_shreds.iter().zip(expected_shreds.iter()) {
                assert_eq!(fetched.index(), expected.index());
                assert_eq!(fetched.payload(), expected.payload());
            }
        }
    }
}

#[test_matrix([true, false], [
    (990, 980, false, false), // update parent before block header -> not dead
    (980, 990, false, true),  // update parent after block header -> dead
    (990, 990, true, true),   // update parent == block header, same id -> dead
    (990, 990, false, false), // update parent == block header, different id -> not dead
])]
fn test_invalid_parent_info_marks_dead(block_header_first: bool, case: (u64, u64, bool, bool)) {
    let (bh_parent_slot, up_parent_slot, same_block_id, expect_dead) = case;
    let slot = 1000;
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let bh_block_id = Hash::new_unique();
    let up_block_id = if same_block_id {
        bh_block_id
    } else {
        Hash::new_unique()
    };

    let block_header_shreds = create_block_header_shreds(slot, bh_parent_slot, bh_block_id);
    let update_parent_shreds = create_update_parent_shreds_with_shred_parent(
        slot,
        bh_parent_slot,
        up_parent_slot,
        up_block_id,
        32,
        false,
    );
    let block_footer_shreds = create_block_footer_shreds(slot, bh_parent_slot, 64);

    let shreds: Vec<Shred> = if block_header_first {
        // Block header shreds first, then update parent, then footer
        let mut s = block_header_shreds;
        s.extend(update_parent_shreds);
        s.extend(block_footer_shreds);
        s
    } else {
        // Update parent shreds first, then block header, then footer
        let mut s = update_parent_shreds;
        s.extend(block_header_shreds);
        s.extend(block_footer_shreds);
        s
    }
    .into_iter()
    .filter(|s| s.is_data())
    .collect();

    blockstore.insert_shreds(shreds, None, true).unwrap();

    assert_eq!(
        blockstore.is_dead(slot),
        expect_dead,
        "block_header_first={block_header_first}, bh_parent={bh_parent_slot}, \
         up_parent={up_parent_slot}, same_block_id={same_block_id}"
    );
    assert_eq!(
        !blockstore.is_full(slot),
        expect_dead,
        "block_header_first={block_header_first}, bh_parent={bh_parent_slot}, \
         up_parent={up_parent_slot}, same_block_id={same_block_id}"
    );
}

#[test]
fn test_invalid_block_header_parent_info_marks_dead() {
    let slot = 1000;
    let shred_parent_slot = slot - 1;

    for block_header_parent_slot in [slot - 2, slot, slot + 1] {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let shreds = create_block_header_shreds_with_shred_parent(
            slot,
            shred_parent_slot,
            block_header_parent_slot,
            Hash::new_unique(),
        );
        blockstore.insert_shreds(shreds, None, false).unwrap();

        assert!(
            blockstore.is_dead(slot),
            "block_header_parent_slot={block_header_parent_slot}"
        );
        assert_ne!(
            blockstore
                .meta(slot)
                .unwrap()
                .and_then(|meta| meta.parent_slot),
            Some(block_header_parent_slot),
            "invalid BlockHeader must not overwrite SlotMeta parent_slot"
        );
    }
}

#[test]
fn test_invalid_update_parent_parent_info_marks_dead() {
    let slot = 1000;
    let shred_parent_slot = slot - 5;

    for update_parent_slot in [shred_parent_slot + 1, slot, slot + 1] {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let mut shreds = create_update_parent_shreds_with_shred_parent(
            slot,
            shred_parent_slot,
            update_parent_slot,
            Hash::new_unique(),
            32,
            false,
        );
        shreds.extend(create_block_footer_shreds(slot, shred_parent_slot, 0));

        blockstore.insert_shreds(shreds, None, false).unwrap();

        assert!(
            blockstore.is_dead(slot),
            "update_parent_slot={update_parent_slot}"
        );
        assert_ne!(
            blockstore
                .meta(slot)
                .unwrap()
                .and_then(|meta| meta.parent_slot),
            Some(update_parent_slot),
            "invalid UpdateParent must not overwrite SlotMeta parent_slot"
        );
    }
}

#[test]
fn test_update_parent_non_first_leader_window_marks_dead() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 10;
    let shred_parent_slot = 5;
    let update_parent_slot = 3;
    let mut shreds = create_block_header_shreds(slot, shred_parent_slot, Hash::new_unique());
    shreds.extend(create_update_parent_shreds_with_shred_parent(
        slot,
        shred_parent_slot,
        update_parent_slot,
        Hash::new_unique(),
        32,
        true,
    ));

    blockstore.insert_shreds(shreds, None, true).unwrap();

    let meta = blockstore.meta(slot).unwrap().unwrap();
    assert!(blockstore.is_dead(slot));
    assert_eq!(meta.parent_slot, Some(shred_parent_slot));
    assert!(!meta.has_update_parent());
}

#[test]
fn test_block_header_followed_by_update_parent() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 12;
    let parent_5_id = Hash::new_unique();
    blockstore
        .insert_shreds(create_block_header_shreds(slot, 5, parent_5_id), None, true)
        .unwrap();

    assert_eq!(blockstore.meta(slot).unwrap().unwrap().parent_slot, Some(5));
    verify_next_slots(&blockstore, 5, &[slot]);

    let parent_3_id = Hash::new_unique();
    blockstore
        .insert_shreds(
            create_update_parent_shreds_with_shred_parent(slot, 5, 3, parent_3_id, 32, true),
            None,
            true,
        )
        .unwrap();

    assert_eq!(blockstore.meta(slot).unwrap().unwrap().parent_slot, Some(3));

    let parent_info = blockstore
        .get_parent_info(slot, BlockLocation::Original)
        .unwrap()
        .unwrap();
    assert_eq!(parent_info.parent_slot, 3);
    assert_eq!(parent_info.parent_block_id, parent_3_id);
    assert!(parent_info.has_update_parent());

    verify_next_slots(&blockstore, 5, &[]);
    verify_next_slots(&blockstore, 3, &[slot]);
}

#[test]
fn test_post_update_orig_after() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 92;
    let original_parent = 85;
    let update_parent = 80;
    blockstore
        .insert_shreds(
            data_shreds(create_block_header_shreds(
                slot,
                original_parent,
                Hash::new_unique(),
            )),
            None,
            false,
        )
        .unwrap();

    let update_parent_block_id = Hash::new_unique();
    blockstore
        .insert_shreds(
            data_shreds(create_update_parent_shreds_with_shred_parent(
                slot,
                original_parent,
                update_parent,
                update_parent_block_id,
                32,
                false,
            )),
            None,
            false,
        )
        .unwrap();

    let meta = blockstore.meta(slot).unwrap().unwrap();
    assert_eq!(meta.parent_slot, Some(update_parent));
    assert_eq!(meta.parent_block_id, update_parent_block_id);
    assert_eq!(meta.replay_fec_set_index, 32);
    assert!(!blockstore.is_dead(slot));

    blockstore
        .insert_shreds(
            data_shreds(create_block_footer_shreds(slot, original_parent, 64)),
            None,
            false,
        )
        .unwrap();

    let meta = blockstore.meta(slot).unwrap().unwrap();
    assert_eq!(meta.parent_slot, Some(update_parent));
    assert_eq!(meta.replay_fec_set_index, 32);
    assert!(blockstore.get_data_shred(slot, 64).unwrap().is_some());
    assert!(!blockstore.is_dead(slot));
}

#[test_matrix([true, false])]
fn test_update_parent_shred_parent(update_parent_first: bool) {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 92;
    let original_parent = 85;
    let update_parent = 80;
    let bad_shred_parent = 84;
    let update_parent_shreds = data_shreds(create_update_parent_shreds_with_shred_parent(
        slot,
        original_parent,
        update_parent,
        Hash::new_unique(),
        32,
        false,
    ));
    let bad_parent_shreds = data_shreds(create_block_footer_shreds_with_last(
        slot,
        bad_shred_parent,
        64,
        false,
    ));

    if update_parent_first {
        blockstore
            .insert_shreds(update_parent_shreds, None, false)
            .unwrap();
        blockstore
            .insert_shreds(
                data_shreds(create_block_footer_shreds_with_last(
                    slot,
                    original_parent,
                    0,
                    false,
                )),
                None,
                false,
            )
            .unwrap();
        assert!(blockstore.meta(slot).unwrap().unwrap().has_update_parent());

        blockstore
            .insert_shreds(bad_parent_shreds, None, false)
            .unwrap();
    } else {
        blockstore
            .insert_shreds(bad_parent_shreds, None, false)
            .unwrap();
        assert_eq!(
            blockstore.meta(slot).unwrap().unwrap().parent_slot,
            Some(bad_shred_parent)
        );

        blockstore
            .insert_shreds(update_parent_shreds, None, false)
            .unwrap();
    }

    assert!(blockstore.is_dead(slot));
}

#[test]
fn test_marker_boundary_ooo() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 96;
    let original_parent = 88;
    let update_parent = 80;
    blockstore
        .insert_shreds(
            data_shreds(create_block_header_shreds(
                slot,
                original_parent,
                Hash::new_unique(),
            )),
            None,
            false,
        )
        .unwrap();

    let mut post_update_shreds = vec![];
    for shred_index in [64, 96, 128] {
        post_update_shreds.extend(data_shreds(create_block_footer_shreds_with_last(
            slot,
            original_parent,
            shred_index,
            false,
        )));
    }
    blockstore
        .insert_shreds(post_update_shreds, None, false)
        .unwrap();

    let meta = blockstore.meta(slot).unwrap().unwrap();
    assert_eq!(meta.parent_slot, Some(original_parent));
    assert_eq!(meta.replay_fec_set_index, 0);

    let update_parent_block_id = Hash::new_unique();
    blockstore
        .insert_shreds(
            data_shreds(create_update_parent_shreds_with_shred_parent(
                slot,
                original_parent,
                update_parent,
                update_parent_block_id,
                32,
                false,
            )),
            None,
            false,
        )
        .unwrap();

    let meta = blockstore.meta(slot).unwrap().unwrap();
    assert_eq!(meta.parent_slot, Some(update_parent));
    assert_eq!(meta.parent_block_id, update_parent_block_id);
    assert_eq!(meta.replay_fec_set_index, 32);
    for shred_index in [64, 96, 128] {
        assert!(
            blockstore
                .get_data_shred(slot, shred_index)
                .unwrap()
                .is_some()
        );
    }
    assert!(!blockstore.is_dead(slot));
}

#[test]
fn test_multiple_children_reparenting() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let parent_id = Hash::new_unique();
    for slot in [44, 40, 48] {
        blockstore
            .insert_shreds(create_block_header_shreds(slot, 35, parent_id), None, true)
            .unwrap();
    }
    verify_next_slots(&blockstore, 35, &[40, 44, 48]);

    blockstore
        .insert_shreds(
            create_update_parent_shreds_with_shred_parent(44, 35, 32, Hash::new_unique(), 32, true),
            None,
            true,
        )
        .unwrap();
    verify_next_slots(&blockstore, 35, &[40, 48]);
    verify_next_slots(&blockstore, 32, &[44]);

    blockstore
        .insert_shreds(
            create_update_parent_shreds_with_shred_parent(40, 35, 33, Hash::new_unique(), 32, true),
            None,
            true,
        )
        .unwrap();
    verify_next_slots(&blockstore, 35, &[48]);
    verify_next_slots(&blockstore, 33, &[40]);
}

#[test]
fn test_interleaved_shred_arrival() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    blockstore
        .insert_shreds(
            create_block_header_shreds(52, 48, Hash::new_unique()),
            None,
            true,
        )
        .unwrap();
    assert_eq!(blockstore.meta(52).unwrap().unwrap().parent_slot, Some(48));

    // Split update parent shreds across two batches
    let mut update_shreds =
        create_update_parent_shreds_with_shred_parent(52, 48, 45, Hash::new_unique(), 32, true);
    let mid = update_shreds.len() / 2;
    let first_half: Vec<_> = update_shreds.drain(..mid).collect();

    blockstore.insert_shreds(first_half, None, true).unwrap();
    blockstore.insert_shreds(update_shreds, None, true).unwrap();

    assert_eq!(blockstore.meta(52).unwrap().unwrap().parent_slot, Some(45));
    verify_next_slots(&blockstore, 48, &[]);
    verify_next_slots(&blockstore, 45, &[52]);
}

#[test]
fn test_same_batch_block_header_then_update_parent() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Insert both BlockHeader and UpdateParent in the same batch,
    // with BlockHeader shreds first (lower indices)
    let mut shreds = create_block_header_shreds(60, 55, Hash::new_unique());
    shreds.extend(create_update_parent_shreds_with_shred_parent(
        60,
        55,
        52,
        Hash::new_unique(),
        32,
        true,
    ));

    blockstore.insert_shreds(shreds, None, true).unwrap();

    // UpdateParent should take precedence
    assert_eq!(blockstore.meta(60).unwrap().unwrap().parent_slot, Some(52));
    verify_next_slots(&blockstore, 55, &[]);
    verify_next_slots(&blockstore, 52, &[60]);
}

#[test]
fn test_same_batch_update_parent_then_block_header() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Insert both UpdateParent and BlockHeader in the same batch,
    // with UpdateParent shreds first (but BlockHeader is at index 0)
    let mut shreds =
        create_update_parent_shreds_with_shred_parent(72, 68, 65, Hash::new_unique(), 32, true);
    shreds.extend(create_block_header_shreds(72, 68, Hash::new_unique()));

    blockstore.insert_shreds(shreds, None, true).unwrap();

    // UpdateParent should take precedence regardless of insertion order
    assert_eq!(blockstore.meta(72).unwrap().unwrap().parent_slot, Some(65));
    verify_next_slots(&blockstore, 68, &[]);
    verify_next_slots(&blockstore, 65, &[72]);
}

#[test]
fn test_multiple_update_parents_out_of_order_marks_dead() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    let slot = 80;
    blockstore
        .insert_shreds(
            data_shreds(create_block_header_shreds(slot, 75, Hash::new_unique())),
            None,
            true,
        )
        .unwrap();

    let first_update_parent_slot = 70;
    let mut first_update_parent_shreds =
        data_shreds(create_update_parent_shreds_with_shred_parent(
            slot,
            75,
            first_update_parent_slot,
            Hash::new_unique(),
            32,
            false,
        ));
    let first_update_parent_marker = first_update_parent_shreds.remove(0);
    assert_eq!(first_update_parent_marker.index(), 32);

    // Insert the tail of the earlier FEC set so the later UpdateParent can
    // be parsed before this UpdateParent marker arrives.
    blockstore
        .insert_shreds(first_update_parent_shreds, None, true)
        .unwrap();

    let second_update_parent_slot = 65;
    blockstore
        .insert_shreds(
            data_shreds(create_update_parent_shreds_with_shred_parent(
                slot,
                75,
                second_update_parent_slot,
                Hash::new_unique(),
                64,
                false,
            )),
            None,
            true,
        )
        .unwrap();

    let parent_info = blockstore
        .get_parent_info(slot, BlockLocation::Original)
        .unwrap()
        .unwrap();
    assert_eq!(parent_info.parent_slot, second_update_parent_slot);
    assert_eq!(parent_info.replay_fec_set_index, 64);
    assert!(!blockstore.is_dead(slot));

    blockstore
        .insert_shreds(vec![first_update_parent_marker], None, true)
        .unwrap();
    assert!(blockstore.is_dead(slot));
}

#[test]
fn test_update_parent_propagates_connectivity() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Slot 0 is connected when full
    let (shreds, _) = make_slot_entries(0, 0, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();
    assert!(blockstore.meta(0).unwrap().unwrap().is_connected());

    // Slot 8 with BlockHeader pointing to disconnected parent
    blockstore
        .insert_shreds(
            create_block_header_shreds(8, 5, Hash::new_unique()),
            None,
            true,
        )
        .unwrap();
    assert!(!blockstore.meta(8).unwrap().unwrap().is_connected());

    // UpdateParent switches to connected parent, slot becomes connected
    blockstore
        .insert_shreds(
            create_update_parent_shreds_with_shred_parent(8, 5, 0, Hash::new_unique(), 32, true),
            None,
            true,
        )
        .unwrap();

    let meta = blockstore.meta(8).unwrap().unwrap();
    assert_eq!(meta.parent_slot, Some(0));
    // Slot 8 is incomplete (not full), so only parent_connected, not connected
    assert!(meta.is_parent_connected());
}

#[test]
fn test_update_parent_propagates_connectivity_to_descendants() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Slot 0 is connected when full
    let (shreds, _) = make_slot_entries(0, 0, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();

    // Slot 100 starts incomplete with BlockHeader pointing to disconnected parent
    blockstore
        .insert_shreds(
            create_block_header_shreds(100, 50, Hash::new_unique()),
            None,
            true,
        )
        .unwrap();
    // Slots 200 and 300 are full, chained to 100
    for (slot, parent) in [(200, 100), (300, 200)] {
        let (shreds, _) = make_slot_entries(slot, parent, 5);
        blockstore.insert_shreds(shreds, None, true).unwrap();
    }
    for slot in [100, 200, 300] {
        assert!(!blockstore.meta(slot).unwrap().unwrap().is_connected());
    }

    // Reparent 100 to connected slot 0; connectivity propagates to 200 and 300
    blockstore
        .insert_shreds(
            create_update_parent_shreds_with_shred_parent(100, 50, 0, Hash::new_unique(), 32, true),
            None,
            true,
        )
        .unwrap();

    // Slot 100 is incomplete, so only parent_connected
    assert!(blockstore.meta(100).unwrap().unwrap().is_parent_connected());
    // Slots 200 and 300 are full, so they become connected
    for slot in [200, 300] {
        assert!(blockstore.meta(slot).unwrap().unwrap().is_connected());
    }
}

#[test]
fn test_update_parent_clears_connectivity() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Slot 0 and 5 are connected (full slots chained together)
    let (shreds, _) = make_slot_entries(0, 0, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();
    let (shreds, _) = make_slot_entries(5, 0, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();
    assert!(blockstore.meta(5).unwrap().unwrap().is_connected());

    // Slot 8 with BlockHeader pointing to connected parent 5
    blockstore
        .insert_shreds(
            create_block_header_shreds(8, 5, Hash::new_unique()),
            None,
            true,
        )
        .unwrap();
    let meta = blockstore.meta(8).unwrap().unwrap();
    assert!(meta.is_parent_connected());

    // Slot 20 chains to slot 8
    let (shreds, _) = make_slot_entries(20, 8, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();

    // UpdateParent switches slot 8 to disconnected parent 3 (lower, doesn't exist)
    blockstore
        .insert_shreds(
            create_update_parent_shreds_with_shred_parent(8, 5, 3, Hash::new_unique(), 32, true),
            None,
            true,
        )
        .unwrap();

    // Slot 8 should have parent_connected cleared
    let meta = blockstore.meta(8).unwrap().unwrap();
    assert_eq!(meta.parent_slot, Some(3));
    assert!(!meta.is_parent_connected());
    // Slot 20 should also have parent_connected cleared
    assert!(!blockstore.meta(20).unwrap().unwrap().is_parent_connected());
}

#[test]
fn test_connectivity_does_not_propagate_through_incomplete_slot() {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    // Slot 0 is the connected root
    let (shreds, _) = make_slot_entries(0, 0, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();
    assert!(blockstore.meta(0).unwrap().unwrap().is_connected());

    // Slot 48 incomplete, pointing to disconnected parent 40
    blockstore
        .insert_shreds(
            create_block_header_shreds(48, 40, Hash::new_unique()),
            None,
            true,
        )
        .unwrap();

    // Full children chain from incomplete slot 48
    let (shreds, _) = make_slot_entries(60, 48, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();
    let (shreds, _) = make_slot_entries(70, 60, 5);
    blockstore.insert_shreds(shreds, None, true).unwrap();

    for slot in [48, 60, 70] {
        assert!(!blockstore.meta(slot).unwrap().unwrap().is_connected());
    }

    // Reparent slot 48 to connected slot 0, keeping it incomplete
    blockstore
        .insert_shreds(
            create_update_parent_shreds_with_shred_parent(48, 40, 0, Hash::new_unique(), 32, false),
            None,
            true,
        )
        .unwrap();

    // Slot 48 incomplete: parent_connected but not connected
    let meta_48 = blockstore.meta(48).unwrap().unwrap();
    assert!(meta_48.is_parent_connected());
    assert!(!meta_48.is_connected());

    // Children stay disconnected since parent 48 is incomplete
    assert!(!blockstore.meta(60).unwrap().unwrap().is_connected());
    assert!(!blockstore.meta(70).unwrap().unwrap().is_connected());
}
