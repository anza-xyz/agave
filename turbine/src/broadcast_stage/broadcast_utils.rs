use {
    super::{Error, Result},
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{self, get_data_shred_bytes_per_batch_typical, ProcessShredsStats},
    },
    solana_poh::poh_recorder::WorkingBankEntry,
    solana_runtime::bank::Bank,
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
    wincode::serialized_size,
};

const ENTRY_COALESCE_DURATION: Duration = Duration::from_millis(50);

pub(super) struct ReceiveResults {
    pub entries: Vec<Entry>,
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

pub(super) fn recv_slot_entries(
    receiver: &Receiver<WorkingBankEntry>,
    carryover_entry: &mut Option<WorkingBankEntry>,
    process_stats: &mut ProcessShredsStats,
) -> Result<ReceiveResults> {
    let recv_start = Instant::now();

    // If there is a carryover entry, use it. Else, see if there is a new entry.
    let (mut bank, (entry, mut last_tick_height)) = match carryover_entry.take() {
        Some((bank, (entry, tick_height))) => (bank, (entry, tick_height)),
        None => receiver.recv_timeout(Duration::new(1, 0))?,
    };
    assert!(last_tick_height <= bank.max_tick_height());
    let mut entries = vec![entry];

    // Drain the channel of entries.
    while last_tick_height != bank.max_tick_height() {
        let Ok((try_bank, (entry, tick_height))) = receiver.try_recv() else {
            break;
        };
        // If the bank changed, that implies the previous slot was interrupted and we do not have to
        // broadcast its entries.
        if try_bank.slot() != bank.slot() {
            warn!("Broadcast for slot: {} interrupted", bank.slot());
            entries.clear();
            bank = try_bank;
        }
        last_tick_height = tick_height;
        entries.push(entry);
        assert!(last_tick_height <= bank.max_tick_height());
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
    // 4. We're "close enough" to tightly packing erasure batches.
    let mut coalesce_start = Instant::now();
    while keep_coalescing_entries(
        last_tick_height,
        bank.max_tick_height(),
        serialized_batch_byte_count,
        max_batch_byte_count,
        process_stats,
    ) {
        let Ok((try_bank, (entry, tick_height))) =
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
            bank = try_bank.clone();
            coalesce_start = Instant::now();
        }
        last_tick_height = tick_height;

        let entry_bytes = serialized_size(&entry)?;
        if serialized_batch_byte_count + entry_bytes > max_batch_byte_count {
            // This entry will push us over the batch byte limit. Save it for
            // the next batch.
            *carryover_entry = Some((try_bank, (entry, tick_height)));
            process_stats.coalesce_exited_hit_max += 1;
            break;
        }

        // Add the entry to the batch.
        serialized_batch_byte_count += entry_bytes;
        entries.push(entry);
        assert!(last_tick_height <= bank.max_tick_height());
    }
    process_stats.receive_elapsed = recv_start.elapsed().as_micros() as u64;
    process_stats.coalesce_elapsed = coalesce_start.elapsed().as_micros() as u64;
    Ok(ReceiveResults {
        entries,
        bank,
        last_tick_height,
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::unbounded,
        solana_genesis_config::GenesisConfig,
        solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        solana_pubkey::Pubkey,
        solana_system_transaction as system_transaction,
        solana_transaction::Transaction,
    };

    fn setup_test() -> (GenesisConfig, Arc<Bank>, Transaction) {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(2);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let tx = system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            1,
            genesis_config.hash(),
        );

        (genesis_config, bank0, tx)
    }

    const LAST_TICK_HEIGHT: u64 = 1;
    const MAX_TICK_HEIGHT: u64 = 10;

    #[test]
    fn test_recv_slot_entries_1() {
        let (genesis_config, bank0, tx) = setup_test();

        let bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));
        let (s, r) = unbounded();
        let mut last_hash = genesis_config.hash();

        assert!(bank1.max_tick_height() > 1);
        let entries: Vec<_> = (1..bank1.max_tick_height() + 1)
            .map(|i| {
                let entry = Entry::new(&last_hash, 1, vec![tx.clone()]);
                last_hash = entry.hash;
                s.send((bank1.clone(), (entry.clone(), i))).unwrap();
                entry
            })
            .collect();

        let mut res_entries = vec![];
        let mut last_tick_height = 0;
        while let Ok(result) = recv_slot_entries(&r, &mut None, &mut ProcessShredsStats::default())
        {
            assert_eq!(result.bank.slot(), bank1.slot());
            last_tick_height = result.last_tick_height;
            res_entries.extend(result.entries);
        }
        assert_eq!(last_tick_height, bank1.max_tick_height());
        assert_eq!(res_entries, entries);
    }

    #[test]
    fn test_recv_slot_entries_2() {
        let (genesis_config, bank0, tx) = setup_test();

        let bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));
        let bank2 = Arc::new(Bank::new_from_parent(bank1.clone(), &Pubkey::default(), 2));
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
                    s.send((bank2.clone(), (entry.clone(), tick_height)))
                        .unwrap();
                    Some(entry)
                } else {
                    s.send((bank1.clone(), (entry, tick_height))).unwrap();
                    None
                }
            })
            .next_back()
            .unwrap()
            .unwrap();

        let mut res_entries = vec![];
        let mut last_tick_height = 0;
        let mut bank_slot = 0;
        while let Ok(result) = recv_slot_entries(&r, &mut None, &mut ProcessShredsStats::default())
        {
            bank_slot = result.bank.slot();
            last_tick_height = result.last_tick_height;
            res_entries = result.entries;
        }
        assert_eq!(bank_slot, bank2.slot());
        assert_eq!(last_tick_height, expected_last_height);
        assert_eq!(res_entries, vec![last_entry]);
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
