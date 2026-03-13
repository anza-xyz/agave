use {
    super::{Error, Result},
    agave_votor::event::{CompletedBlock, VotorEvent, VotorEventSender},
    agave_votor_messages::migration::MigrationStatus,
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_entry::{block_component::BlockComponent, entry::Entry, entry_or_marker::EntryOrMarker},
    solana_hash::Hash,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{self, ProcessShredsStats, get_data_shred_bytes_per_batch_typical},
    },
    solana_poh::poh_recorder::WorkingBankEntryOrMarker,
    solana_runtime::bank::Bank,
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
    wincode::serialized_size,
};

const ENTRY_COALESCE_DURATION: Duration = Duration::from_millis(50);

pub(super) struct ReceiveResults {
    pub component: BlockComponent,
    pub bank: Arc<Bank>,
    pub last_tick_height: u64,
}

const fn get_target_batch_bytes_default() -> u64 {
    // Empirically discovered to be a good balance between avoiding padding and
    // not delaying broadcast.
    2 * get_data_shred_bytes_per_batch_typical()
}

const fn get_target_batch_pad_bytes() -> u64 {
    // Less than 5% padding is acceptable overhead. Let's not push our luck.
    get_data_shred_bytes_per_batch_typical() / 20
}

fn keep_coalescing_entries(
    last_tick_height: u64,
    max_tick_height: u64,
    serialized_batch_byte_count: u64,
    max_batch_byte_count: u64,
    process_stats: &mut ProcessShredsStats,
) -> bool {
    if last_tick_height >= max_tick_height {
        // The slot has ended.
        process_stats.coalesce_exited_slot_ended += 1;
        return false;
    } else if serialized_batch_byte_count >= max_batch_byte_count {
        // Exceeded the max batch byte count.
        process_stats.coalesce_exited_hit_max += 1;
        return false;
    }
    let typical = get_data_shred_bytes_per_batch_typical();
    let remaining_bytes = serialized_batch_byte_count % typical;
    let bytes_to_fill_erasure_batch = if remaining_bytes == 0 {
        0
    } else {
        typical - remaining_bytes
    };
    if bytes_to_fill_erasure_batch < get_target_batch_pad_bytes() {
        // We're close enough to tightly packing erasure batches. Just send it.
        process_stats.coalesce_exited_tightly_packed += 1;
        return false;
    }
    true
}

pub(super) fn recv_slot_components(
    receiver: &Receiver<WorkingBankEntryOrMarker>,
    carryover_entry: &mut Option<WorkingBankEntryOrMarker>,
    process_stats: &mut ProcessShredsStats,
) -> Result<ReceiveResults> {
    loop {
        if let Some(result) =
            recv_slot_components_maybe_empty(receiver, carryover_entry, process_stats)?
        {
            return Ok(result);
        }
    }
}

fn recv_slot_components_maybe_empty(
    receiver: &Receiver<WorkingBankEntryOrMarker>,
    carryover_entry: &mut Option<WorkingBankEntryOrMarker>,
    process_stats: &mut ProcessShredsStats,
) -> Result<Option<ReceiveResults>> {
    let recv_start = Instant::now();

    // If there is a carryover entry, use it. Else, see if there is a new entry.
    let (mut bank, (entry_or_marker, mut last_tick_height)) = match carryover_entry.take() {
        Some((bank, (entry_or_marker, tick_height))) => (bank, (entry_or_marker, tick_height)),
        None => receiver.recv_timeout(Duration::new(1, 0))?,
    };
    assert!(last_tick_height <= bank.max_tick_height());

    let mut entries: Vec<Entry> = match entry_or_marker {
        EntryOrMarker::Marker(marker) => {
            // If the first thing is a block marker, return it immediately
            process_stats.receive_elapsed = recv_start.elapsed().as_micros() as u64;

            return Ok(Some(ReceiveResults {
                component: BlockComponent::BlockMarker(marker),
                bank,
                last_tick_height,
            }));
        }
        EntryOrMarker::Entry(entry) => {
            vec![entry]
        }
    };

    // Drain the channel of entries until we hit a marker or the slot ends
    let mut hit_marker = false;
    while last_tick_height != bank.max_tick_height() {
        let Ok((try_bank, (next_entry_or_marker, tick_height))) = receiver.try_recv() else {
            break;
        };
        // If the bank changed, that implies the previous slot was interrupted and we do not have to
        // broadcast its entries.
        if try_bank.slot() != bank.slot() {
            warn!("Broadcast for slot: {} interrupted", bank.slot());
            entries.clear();
            last_tick_height = 0;
            bank = try_bank.clone();
        }
        match next_entry_or_marker {
            EntryOrMarker::Marker(ref _marker) => {
                // If we hit a block marker, save it for next time and stop draining
                *carryover_entry = Some((try_bank, (next_entry_or_marker, tick_height)));
                hit_marker = true;
                break;
            }
            EntryOrMarker::Entry(entry) => {
                // Only update after confirming the entry is included in this batch.
                last_tick_height = tick_height;

                // Add the entry to our batch
                entries.push(entry);
                assert!(last_tick_height <= bank.max_tick_height());
            }
        }
    }

    let mut serialized_batch_byte_count = serialized_size(&entries)?;

    // Determine the maximum batch size we will allow for coalescing. Normally
    // this would just be the default target batch size, but if we happened to
    // pull out a large number of entries from the channel, we might have
    // already exceeded the default target, and we should try to build towards the
    // next batch boundary to avoid excessive padding.
    let next_full_batch_byte_count = serialized_batch_byte_count
        .div_ceil(get_data_shred_bytes_per_batch_typical())
        .saturating_mul(get_data_shred_bytes_per_batch_typical());
    let max_batch_byte_count = next_full_batch_byte_count.max(get_target_batch_bytes_default());

    // Coalesce entries until one of the following conditions are hit:
    // 1. We ticked through the entire slot.
    // 2. We hit the timeout.
    // 3. We're over the max data target.
    // 4. We hit a block marker.
    // 5. We're "close enough" to tightly packing erasure batches.
    let mut coalesce_start = Instant::now();
    while !hit_marker
        && keep_coalescing_entries(
            last_tick_height,
            bank.max_tick_height(),
            serialized_batch_byte_count,
            max_batch_byte_count,
            process_stats,
        )
    {
        let Ok((try_bank, (entry_or_marker, tick_height))) =
            receiver.recv_deadline(coalesce_start + ENTRY_COALESCE_DURATION)
        else {
            process_stats.coalesce_exited_rcv_timeout += 1;
            break;
        };
        // If the bank changed, that implies the previous slot was interrupted and we do not have to
        // broadcast its entries.
        if try_bank.slot() != bank.slot() {
            warn!("Broadcast for slot: {} interrupted", bank.slot());
            entries.clear();
            serialized_batch_byte_count = 8; // Vec len
            last_tick_height = 0;
            bank = try_bank.clone();
            coalesce_start = Instant::now();
            debug_assert!(carryover_entry.is_none());
        }

        match entry_or_marker {
            EntryOrMarker::Marker(ref _marker) => {
                // If we hit a block marker, save it for next time and stop coalescing
                *carryover_entry = Some((try_bank, (entry_or_marker, tick_height)));
                break;
            }
            EntryOrMarker::Entry(entry) => {
                let entry_bytes = serialized_size(&entry)?;

                if serialized_batch_byte_count + entry_bytes > max_batch_byte_count {
                    // This entry will push us over the batch byte limit. Save it for
                    // the next batch.
                    *carryover_entry = Some((try_bank, (entry.into(), tick_height)));
                    process_stats.coalesce_exited_hit_max += 1;
                    break;
                }

                // only update the last tick height after confirming we did
                // not carry over the entry to the next batch.
                last_tick_height = tick_height;

                // Add the entry to the batch.
                serialized_batch_byte_count += entry_bytes;
                entries.push(entry);
            }
        }

        assert!(last_tick_height <= bank.max_tick_height());
    }
    process_stats.receive_elapsed = recv_start.elapsed().as_micros() as u64;
    process_stats.coalesce_elapsed = coalesce_start.elapsed().as_micros() as u64;

    Ok(match entries.is_empty() {
        true => None,
        false => Some(ReceiveResults {
            component: BlockComponent::EntryBatch(entries),
            bank,
            last_tick_height,
        }),
    })
}

// Returns the Merkle root of the last erasure batch of the parent slot.
pub(super) fn get_chained_merkle_root_from_parent(
    slot: Slot,
    parent: Slot,
    blockstore: &Blockstore,
) -> Result<Hash> {
    if slot == parent {
        debug_assert_eq!(slot, 0u64);
        return Ok(Hash::default());
    }
    debug_assert!(parent < slot, "parent: {parent} >= slot: {slot}");
    let index = blockstore
        .meta(parent)?
        .ok_or(Error::UnknownSlotMeta(parent))?
        .last_index
        .ok_or(Error::UnknownLastIndex(parent))?;
    let shred = blockstore
        .get_data_shred(parent, index)?
        .ok_or(Error::ShredNotFound {
            slot: parent,
            index,
        })?;
    shred::layout::get_merkle_root(&shred).ok_or(Error::InvalidMerkleRoot {
        slot: parent,
        index,
    })
}

/// Set the block id on the bank and send it for consideration in voting
pub(super) fn set_block_id_and_send(
    migration_status: &MigrationStatus,
    votor_event_sender: &VotorEventSender,
    bank: Arc<Bank>,
    block_id: Hash,
) -> Result<()> {
    bank.set_block_id(Some(block_id));
    if bank.is_frozen() && migration_status.should_send_votor_event(bank.slot()) {
        votor_event_sender.send(VotorEvent::Block(CompletedBlock {
            slot: bank.slot(),
            bank,
        }))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::unbounded,
        solana_entry::entry_or_marker::EntryOrMarker,
        solana_genesis_config::GenesisConfig,
        solana_ledger::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_runtime::bank::SlotLeader,
        solana_system_transaction as system_transaction,
        solana_transaction::Transaction,
    };

    fn setup_test() -> (
        GenesisConfig,
        Arc<Bank>,
        Arc<std::sync::RwLock<solana_runtime::bank_forks::BankForks>>,
        Transaction,
    ) {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(2);
        let (bank0, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let tx = system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            1,
            genesis_config.hash(),
        );

        (genesis_config, bank0, bank_forks, tx)
    }

    const LAST_TICK_HEIGHT: u64 = 1;
    const MAX_TICK_HEIGHT: u64 = 10;

    #[test]
    fn test_recv_slot_components_1() {
        let (genesis_config, bank0, bank_forks, tx) = setup_test();

        let bank1 = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank0,
            SlotLeader::default(),
            1,
        );
        let (s, r) = unbounded();
        let mut last_hash = genesis_config.hash();

        assert!(bank1.max_tick_height() > 1);
        let entries: Vec<_> = (1..bank1.max_tick_height() + 1)
            .map(|i| {
                let entry = Entry::new(&last_hash, 1, vec![tx.clone()]);
                last_hash = entry.hash;
                s.send((bank1.clone(), (EntryOrMarker::Entry(entry.clone()), i)))
                    .unwrap();
                entry
            })
            .collect();

        let mut res_entries = vec![];
        let mut last_tick_height = 0;
        while let Ok(result) =
            recv_slot_components(&r, &mut None, &mut ProcessShredsStats::default())
        {
            assert_eq!(result.bank.slot(), bank1.slot());
            last_tick_height = result.last_tick_height;
            if let BlockComponent::EntryBatch(entries) = result.component {
                res_entries.extend(entries);
            }
        }
        assert_eq!(last_tick_height, bank1.max_tick_height());
        assert_eq!(res_entries, entries);
    }

    #[test]
    fn test_recv_slot_components_2() {
        let (genesis_config, bank0, bank_forks, tx) = setup_test();

        let bank1 = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank0,
            SlotLeader::default(),
            1,
        );
        let bank2 = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank1.clone(),
            SlotLeader::default(),
            2,
        );
        let (s, r) = unbounded();

        let mut last_hash = genesis_config.hash();
        assert!(bank1.max_tick_height() > 1);
        // Simulate slot 2 interrupting slot 1's transmission
        let expected_last_height = bank1.max_tick_height();
        let last_entry = (1..=bank1.max_tick_height())
            .map(|tick_height| {
                let entry = Entry::new(&last_hash, 1, vec![tx.clone()]);
                last_hash = entry.hash;
                // Interrupt slot 1 right before the last tick
                if tick_height == expected_last_height {
                    s.send((
                        bank2.clone(),
                        (EntryOrMarker::Entry(entry.clone()), tick_height),
                    ))
                    .unwrap();
                    Some(entry)
                } else {
                    s.send((bank1.clone(), (EntryOrMarker::Entry(entry), tick_height)))
                        .unwrap();
                    None
                }
            })
            .next_back()
            .unwrap()
            .unwrap();

        let mut res_entries = vec![];
        let mut last_tick_height = 0;
        let mut bank_slot = 0;
        while let Ok(result) =
            recv_slot_components(&r, &mut None, &mut ProcessShredsStats::default())
        {
            bank_slot = result.bank.slot();
            last_tick_height = result.last_tick_height;
            if let BlockComponent::EntryBatch(entries) = result.component {
                res_entries = entries;
            }
        }
        assert_eq!(bank_slot, bank2.slot());
        assert_eq!(last_tick_height, expected_last_height);
        assert_eq!(res_entries, vec![last_entry]);
    }

    #[test]
    fn test_marker_carryover_does_not_advance_last_tick_height() {
        let (genesis_config, bank0, _bank_forks, tx) = setup_test();
        let bank1 = Arc::new(Bank::new_from_parent(bank0, SlotLeader::default(), 1));
        let (s, r) = unbounded();
        let max_tick = bank1.max_tick_height();

        let mut last_hash = genesis_config.hash();
        for tick in 1..=2 {
            let entry = Entry::new(&last_hash, 1, vec![tx.clone()]);
            last_hash = entry.hash;
            s.send((bank1.clone(), (EntryOrMarker::Entry(entry), tick)))
                .unwrap();
        }

        let marker = solana_entry::block_component::VersionedBlockMarker::new_block_header(
            solana_entry::block_component::BlockHeaderV1 {
                parent_slot: 0,
                parent_block_id: Hash::default(),
            },
        );
        s.send((bank1.clone(), (EntryOrMarker::Marker(marker), max_tick)))
            .unwrap();

        let mut carryover = None;
        let result =
            recv_slot_components(&r, &mut carryover, &mut ProcessShredsStats::default()).unwrap();

        assert!(matches!(result.component, BlockComponent::EntryBatch(ref e) if e.len() == 2));
        assert_eq!(result.last_tick_height, 2);
        assert!(carryover.is_some());

        let result =
            recv_slot_components(&r, &mut carryover, &mut ProcessShredsStats::default()).unwrap();
        assert!(matches!(result.component, BlockComponent::BlockMarker(_)));
        assert_eq!(result.last_tick_height, max_tick);
    }

    #[test]
    fn test_marker_preserves_entry_ordering() {
        let (genesis_config, bank0, _bank_forks, tx) = setup_test();
        let bank1 = Arc::new(Bank::new_from_parent(bank0, SlotLeader::default(), 1));
        let (s, r) = unbounded();

        let mut last_hash = genesis_config.hash();

        let entry1 = Entry::new(&last_hash, 1, vec![tx.clone()]);
        last_hash = entry1.hash;
        s.send((bank1.clone(), (EntryOrMarker::Entry(entry1.clone()), 1)))
            .unwrap();

        let marker = solana_entry::block_component::VersionedBlockMarker::new_block_header(
            solana_entry::block_component::BlockHeaderV1 {
                parent_slot: 0,
                parent_block_id: Hash::default(),
            },
        );
        s.send((bank1.clone(), (EntryOrMarker::Marker(marker), 2)))
            .unwrap();

        let entry2 = Entry::new(&last_hash, 1, vec![tx.clone()]);
        s.send((bank1.clone(), (EntryOrMarker::Entry(entry2.clone()), 3)))
            .unwrap();

        let mut carryover = None;

        // First call should return only entry1
        let result =
            recv_slot_components(&r, &mut carryover, &mut ProcessShredsStats::default()).unwrap();
        assert!(matches!(result.component, BlockComponent::EntryBatch(ref e) if e.len() == 1));
        if let BlockComponent::EntryBatch(ref entries) = result.component {
            assert_eq!(entries[0], entry1);
        }
        assert_eq!(result.last_tick_height, 1);

        // Second call should return the marker
        let result =
            recv_slot_components(&r, &mut carryover, &mut ProcessShredsStats::default()).unwrap();
        assert!(matches!(result.component, BlockComponent::BlockMarker(_)));
        assert_eq!(result.last_tick_height, 2);

        // Third call should return entry2
        let result =
            recv_slot_components(&r, &mut carryover, &mut ProcessShredsStats::default()).unwrap();
        assert!(matches!(result.component, BlockComponent::EntryBatch(ref e) if e.len() == 1));
        if let BlockComponent::EntryBatch(ref entries) = result.component {
            assert_eq!(entries[0], entry2);
        }
        assert_eq!(result.last_tick_height, 3);
    }

    #[test]
    fn test_bank_change_then_marker_skips_empty_entry_batch() {
        let (genesis_config, bank0, _bank_forks, tx) = setup_test();
        let bank1 = Arc::new(Bank::new_from_parent(
            bank0.clone(),
            SlotLeader::default(),
            1,
        ));
        let bank2 = Arc::new(Bank::new_from_parent(
            bank1.clone(),
            SlotLeader::default(),
            2,
        ));
        let (s, r) = unbounded();

        let mut last_hash = genesis_config.hash();

        // Send one entry for bank1 with tick_height=5
        let entry = Entry::new(&last_hash, 1, vec![tx.clone()]);
        last_hash = entry.hash;
        s.send((bank1.clone(), (EntryOrMarker::Entry(entry), 5)))
            .unwrap();

        // Bank changes to bank2, but the next item is a marker with tick_height=3 — this should
        // leave entries empty after the clear. The stale last_tick_height (5, from bank1) must not
        // leak into subsequent results.
        let marker = solana_entry::block_component::VersionedBlockMarker::new_block_header(
            solana_entry::block_component::BlockHeaderV1 {
                parent_slot: 1,
                parent_block_id: Hash::default(),
            },
        );
        s.send((bank2.clone(), (EntryOrMarker::Marker(marker), 3)))
            .unwrap();

        // Ensure that the inner function returns None when the channel has no more items after the
        // bank change + marker.
        let mut carryover = None;
        let result = recv_slot_components_maybe_empty(
            &r,
            &mut carryover,
            &mut ProcessShredsStats::default(),
        )
        .unwrap();
        assert!(result.is_none());
        assert!(carryover.is_some());

        // Now send a real entry for bank2 so recv_slot_components has something to return after
        // skipping the empty batch.
        let entry2 = Entry::new(&last_hash, 1, vec![tx.clone()]);
        s.send((bank2.clone(), (EntryOrMarker::Entry(entry2.clone()), 2)))
            .unwrap();

        // Verify that the outer function skips the empty batch and returns the carried-over marker.
        // last_tick_height must be 3 (from the marker), not 5 (stale value from bank1).
        let result =
            recv_slot_components(&r, &mut carryover, &mut ProcessShredsStats::default()).unwrap();
        assert!(matches!(result.component, BlockComponent::BlockMarker(_)));
        assert_eq!(result.last_tick_height, 3);
    }

    #[test]
    fn test_keep_coalescing_exact_boundary_exits() {
        let typical = get_data_shred_bytes_per_batch_typical();
        let mut stats = ProcessShredsStats::default();
        let serialized = typical * 2; // exact boundary
        let max_batch = typical * 4; // still below max
        let keep = keep_coalescing_entries(
            LAST_TICK_HEIGHT,
            MAX_TICK_HEIGHT,
            serialized,
            max_batch,
            &mut stats,
        );
        assert!(!keep);
        assert_eq!(stats.coalesce_exited_tightly_packed, 1);
    }

    #[test]
    fn test_keep_coalescing_near_boundary_exits() {
        let typical = get_data_shred_bytes_per_batch_typical();
        let pad = get_target_batch_pad_bytes();
        assert!(pad > 0);
        let mut stats = ProcessShredsStats::default();
        // bytes_to_fill = pad - 1 -> ensure early exit
        let rem = typical - (pad - 1);
        let serialized = typical * 3 + rem;
        let keep = keep_coalescing_entries(
            LAST_TICK_HEIGHT,
            MAX_TICK_HEIGHT,
            serialized,
            typical * 10,
            &mut stats,
        );
        assert!(!keep);
        assert_eq!(stats.coalesce_exited_tightly_packed, 1);
    }

    #[test]
    fn test_keep_coalescing_not_close_enough_continues() {
        let typical = get_data_shred_bytes_per_batch_typical();
        let pad = get_target_batch_pad_bytes();
        let mut stats = ProcessShredsStats::default();
        // bytes_to_fill = pad -> should continue
        let rem = typical - pad;
        let serialized = typical + rem;
        let keep = keep_coalescing_entries(
            LAST_TICK_HEIGHT,
            MAX_TICK_HEIGHT,
            serialized,
            typical * 10,
            &mut stats,
        );
        assert!(keep);
        assert_eq!(stats.coalesce_exited_tightly_packed, 0);
    }

    #[test]
    fn test_keep_coalescing_hit_max() {
        let typical = get_data_shred_bytes_per_batch_typical();
        let mut stats = ProcessShredsStats::default();
        let serialized = typical * 4;
        let max_batch = typical * 4; // >= triggers hit_max
        let keep = keep_coalescing_entries(
            LAST_TICK_HEIGHT,
            MAX_TICK_HEIGHT,
            serialized,
            max_batch,
            &mut stats,
        );
        assert!(!keep);
        assert_eq!(stats.coalesce_exited_hit_max, 1);
    }

    #[test]
    fn test_keep_coalescing_slot_ended() {
        let mut stats = ProcessShredsStats::default();
        let keep =
            keep_coalescing_entries(MAX_TICK_HEIGHT, MAX_TICK_HEIGHT, 0, 1_000_000, &mut stats);
        assert!(!keep);
        assert_eq!(stats.coalesce_exited_slot_ended, 1);
    }
}
