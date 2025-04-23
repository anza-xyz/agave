use {
    super::{Error, Result},
    bincode::serialized_size,
    crossbeam_channel::Receiver,
    solana_entry::entry::Entry,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{self, get_data_shred_bytes_per_batch_typical, ProcessShredsStats},
    },
    solana_poh::poh_recorder::WorkingBankEntry,
    solana_runtime::bank::Bank,
    solana_sdk::{clock::Slot, hash::Hash},
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
};

const ENTRY_COALESCE_DURATION: Duration = Duration::from_millis(50);

pub(super) struct ReceiveResults {
    pub entries: Vec<Entry>,
    pub bank: Arc<Bank>,
    pub last_tick_height: u64,
}

fn data_shred_bytes_per_batch() -> u64 {
    *get_data_shred_bytes_per_batch_typical()
}

fn target_batch_bytes_default() -> u64 {
    // Empirically discovered to be a good balance between avoiding padding and
    // not delaying broadcast.
    3 * data_shred_bytes_per_batch()
}

fn target_batch_pad_bytes() -> f64 {
    // Less than 5% padding is acceptable overhead. Let's not push our luck.
    data_shred_bytes_per_batch() as f64 * 0.05
}

fn keep_coalescing_entries(
    last_tick_height: u64,
    max_tick_height: u64,
    serialized_batch_byte_count: u64,
    max_batch_byte_count: u64,
) -> bool {
    // This slot is not over.
    last_tick_height < max_tick_height &&
    // We have not exceeded max batch byte count.
    serialized_batch_byte_count < max_batch_byte_count &&
    {
        let bytes_to_fill_erasure_batch = data_shred_bytes_per_batch() - (serialized_batch_byte_count % data_shred_bytes_per_batch());
        // We haven't tightly packed this batch.
        (bytes_to_fill_erasure_batch as f64) > target_batch_pad_bytes()
    }
}

// Dynamically determine the coalesce time based on the amount of entry data
// that has been built up. If we only have a small amount, we can afford to wait
// longer and avoid unnecessary padding. If we have a lot, we shouldn't wait as
// long so we avoid delaying downstream validator replay.
fn max_coalesce_time(serialized_batch_byte_count: u64, max_batch_byte_count: u64) -> Duration {
    assert!(max_batch_byte_count > 0);
    // Compute the fraction of the target batch that has been filled.
    // Constrain to 75% to ensure we allow some time for coalescing.
    let ratio = (serialized_batch_byte_count as f32 / max_batch_byte_count as f32).min(0.75);
    ENTRY_COALESCE_DURATION.mul_f32(1.0 - ratio)
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
    let next_full_batch_byte_count = serialized_batch_byte_count
        .div_ceil(data_shred_bytes_per_batch())
        .saturating_mul(data_shred_bytes_per_batch());
    let max_batch_byte_count = next_full_batch_byte_count.max(target_batch_bytes_default());

    // Coalesce entries until one of the following conditions are hit:
    // 1. We ticked through the entire slot.
    // 2. We hit the timeout.
    // 3. We're over the max data target.
    let mut coalesce_start = Instant::now();
    while keep_coalescing_entries(
        last_tick_height,
        bank.max_tick_height(),
        serialized_batch_byte_count,
        max_batch_byte_count,
    ) {
        let Ok((try_bank, (entry, tick_height))) = receiver.recv_deadline(
            coalesce_start + max_coalesce_time(serialized_batch_byte_count, max_batch_byte_count),
        ) else {
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
        solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        solana_sdk::{
            genesis_config::GenesisConfig, pubkey::Pubkey, system_transaction,
            transaction::Transaction,
        },
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
            &solana_sdk::pubkey::new_rand(),
            1,
            genesis_config.hash(),
        );

        (genesis_config, bank0, tx)
    }

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
}
