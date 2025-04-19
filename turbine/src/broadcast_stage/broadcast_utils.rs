use {
    super::{Error, Result},
    bincode::serialized_size,
    crossbeam_channel::Receiver,
    solana_entry::entry::Entry,
    solana_ledger::{
        blockstore::Blockstore,
        shred::{self, ProcessShredsStats, ShredData},
    },
    solana_poh::poh_recorder::WorkingBankEntry,
    solana_runtime::bank::Bank,
    solana_sdk::{clock::Slot, hash::Hash},
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
};

const ENTRY_COALESCE_DURATION: Duration = Duration::from_millis(200);

pub(super) struct ReceiveResults {
    pub entries: Vec<Entry>,
    pub bank: Arc<Bank>,
    pub last_tick_height: u64,
}

fn keep_coalescing_entries(
    last_tick_height: u64,
    max_tick_height: u64,
    serialized_batch_byte_count: u64,
    max_batch_byte_count: u64,
    data_shred_bytes_per_batch: u64,
    process_stats: &mut ProcessShredsStats,
) -> bool {
    // If we are at the last tick height, we don't need to coalesce anymore.
    if last_tick_height >= max_tick_height {
        process_stats.coalesce_exited_slot_ended += 1;
        return false;
    } else if serialized_batch_byte_count >= max_batch_byte_count {
        // If we are over the max batch byte count, we don't need to coalesce anymore.
        process_stats.coalesce_exited_hit_max += 1;
        return false;
    }
    let bytes_to_fill_erasure_batch =
        data_shred_bytes_per_batch - (serialized_batch_byte_count % data_shred_bytes_per_batch);
    if (bytes_to_fill_erasure_batch as f64) < (data_shred_bytes_per_batch as f64 * 0.05) {
        // We're close enough to tightly packing erasure batches. Just send it.
        process_stats.coalesce_exited_tightly_packed += 1;
        return false;
    }
    true
}

fn max_coalesce_time(serialized_batch_byte_count: u64, max_batch_byte_count: u64) -> Duration {
    // Compute the fraction of the target batch that has been filled.
    let ratio = (serialized_batch_byte_count as f64 / max_batch_byte_count as f64).min(0.75);

    // Scale the base duration: the more data we have (ratio near 1.0), the less we wait.
    ENTRY_COALESCE_DURATION.mul_f64(1.0 - ratio)
}

pub(super) fn recv_slot_entries(
    receiver: &Receiver<WorkingBankEntry>,
    carryover_entry: &mut Option<WorkingBankEntry>,
    process_stats: &mut ProcessShredsStats,
) -> Result<ReceiveResults> {
    let timer = Duration::new(1, 0);
    let recv_start = Instant::now();

    // If there is a carryover entry, use it. Else, see if there is a new entry.
    let (mut bank, (entry, mut last_tick_height)) = match carryover_entry.take() {
        Some((bank, (entry, tick_height))) => (bank, (entry, tick_height)),
        None => receiver.recv_timeout(timer)?,
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
    let data_shred_bytes =
        ShredData::capacity(Some((6, true, false))).expect("Failed to get capacity") as u64;
    let data_shred_bytes_per_batch = 32 * data_shred_bytes;
    let next_full_batch_byte_count = serialized_batch_byte_count
        .div_ceil(data_shred_bytes_per_batch)
        .saturating_mul(data_shred_bytes_per_batch);
    let max_batch_byte_count =
        std::cmp::max(3 * data_shred_bytes_per_batch, next_full_batch_byte_count);

    // Coalesce entries until one of the following conditions are hit:
    // 1. We have neatly packed some multiple of data batches (minimizes padding).
    // 2. We hit the timeout.
    // 3. Next entry would push us over the max data limit.
    let mut coalesce_start = Instant::now();
    while keep_coalescing_entries(
        last_tick_height,
        bank.max_tick_height(),
        serialized_batch_byte_count,
        max_batch_byte_count,
        data_shred_bytes_per_batch,
        process_stats,
    ) {
        // Fetch the next entry.
        let Ok((try_bank, (entry, tick_height))) = receiver.recv_deadline(
            coalesce_start + max_coalesce_time(serialized_batch_byte_count, max_batch_byte_count),
        ) else {
            process_stats.coalesce_exited_rcv_timeout += 1;
            break;
        };

        if try_bank.slot() != bank.slot() {
            // The bank changed, that implies the previous slot was interrupted
            // and we do not have to broadcast its entries.
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
        rand::Rng,
        solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        solana_sdk::{
            genesis_config::GenesisConfig, pubkey::Pubkey, system_transaction,
            transaction::Transaction,
        },
        solana_vote::vote_transaction::new_compact_vote_state_update_transaction,
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

    #[test]
    fn test_recv_slot_entries_3() {
        solana_logger::setup();
        let (genesis_config, bank0, mut tx) = setup_test();
        // Add some random keys to the transaction to make it larger. This is to
        // make transaction size similar to what is observed on mainnet (~700B).
        for _ in 0..15 {
            tx.message.account_keys.push(solana_sdk::pubkey::new_rand());
        }
        let last_tick_height = bank0.max_tick_height();
        let (entry_sender, entry_receiver) = unbounded();
        let genesis_config_hash = genesis_config.hash();

        // These constants were selected to mimic typical counts per slot seen
        // on mainnet.
        const TX_COUNT_LIMIT: usize = 400;
        const VOTE_COUNT_LIMIT: usize = 1300;

        // Send entries into the channel to be coalesced.
        let sender_thread = std::thread::spawn(move || {
            let mut last_hash = genesis_config_hash;
            let mut tx_send_count = 0;
            let mut vote_send_count = 0;
            let vote_tx = new_compact_vote_state_update_transaction(
                vec![(0, 0); 32].into(),
                Hash::default(),
                &solana_sdk::signature::Keypair::new(),
                &solana_sdk::signature::Keypair::new(),
                &solana_sdk::signature::Keypair::new(),
                None,
            );
            let mut rng = rand::thread_rng();
            let mut tick = bank0.tick_height();
            let start_time = Instant::now();

            // Send an entire slot's worth of entries.
            while tick < last_tick_height {
                tick = (Instant::now()
                    .duration_since(start_time)
                    .as_micros()
                    .saturating_div(genesis_config.poh_config.target_tick_duration.as_micros())
                    as u64)
                    .min(last_tick_height);
                let (entry, sleep_duration) = match (
                    tx_send_count > TX_COUNT_LIMIT,
                    vote_send_count > VOTE_COUNT_LIMIT,
                    rng.gen_range(0..=1),
                ) {
                    (true, true, _) => {
                        // Ticks only
                        (
                            Entry::new_tick(1, &last_hash),
                            genesis_config.poh_config.target_tick_duration,
                        )
                    }
                    (true, false, _) | (false, false, 0) => {
                        // Vote Txs
                        let entry_vote_count = rng.gen_range(8..=16);
                        vote_send_count += entry_vote_count;
                        let sleep_micros = rng.gen_range(50..=100);
                        (
                            Entry::new(&last_hash, 1, vec![vote_tx.clone(); entry_vote_count]),
                            Duration::from_micros(sleep_micros),
                        )
                    }
                    (false, true, _) | (false, false, 1) => {
                        // Non-vote Txs
                        let entry_tx_count = rng.gen_range(1..=3);
                        tx_send_count += entry_tx_count;
                        let sleep_micros = if vote_send_count > VOTE_COUNT_LIMIT {
                            rng.gen_range(100..=2000)
                        } else {
                            rng.gen_range(50..=100)
                        };
                        (
                            Entry::new(&last_hash, 1, vec![tx.clone(); entry_tx_count]),
                            Duration::from_micros(sleep_micros),
                        )
                    }
                    _ => unreachable!(),
                };
                last_hash = entry.hash;
                entry_sender.send((bank0.clone(), (entry, tick))).unwrap();
                // Sleep to simulate delays in getting txs/entries through the TPU.
                std::thread::sleep(sleep_duration);
            }
        });

        let data_shred_bytes =
            ShredData::capacity(Some((6, true, false))).expect("Failed to get capacity") as u64;
        let data_shred_bytes_per_batch = 32 * data_shred_bytes;
        let mut data_bytes = 0;
        let mut pad_bytes = 0;
        let mut carryover_entry = None;
        let mut process_stats = ProcessShredsStats::default();
        loop {
            match recv_slot_entries(&entry_receiver, &mut carryover_entry, &mut process_stats) {
                Ok(result) => {
                    let data_byte_count: u64 = bincode::serialize(&result.entries)
                        .unwrap()
                        .len()
                        .try_into()
                        .unwrap();
                    let pad_byte_count =
                        data_shred_bytes_per_batch - (data_byte_count % data_shred_bytes_per_batch);
                    data_bytes += data_byte_count;
                    pad_bytes += pad_byte_count;
                    if result.last_tick_height == last_tick_height {
                        // End of slot
                        break;
                    }
                }
                Err(Error::RecvTimeout(_)) => {
                    // This is expected, just continue to the next iteration
                    continue;
                }
                Err(e) => {
                    error!("recv_slot_entries error: {:?}", e);
                    break;
                }
            }
        }

        // Anything more than 14% padding is excessive and should be understood.
        let pad_pct = (pad_bytes as f64) / (pad_bytes as f64 + data_bytes as f64);
        info!("pad_pct: {}", pad_pct);
        assert!(pad_pct < 0.14);

        sender_thread.join().unwrap();
    }
}
