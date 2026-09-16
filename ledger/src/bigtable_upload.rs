use {
    crate::blockstore::{
        Blockstore, ConfirmedBlockComponent, VersionedConfirmedBlockWithComponents,
    },
    crossbeam_channel::{bounded, unbounded},
    log::*,
    solana_clock::Slot,
    solana_entry::block_component::{BlockHeaderV1, VersionedBlockMarker},
    solana_measure::measure::Measure,
    solana_transaction_status::{EntrySummary, VersionedConfirmedBlockWithSplitComponents},
    std::{
        cmp::{max, min},
        collections::HashSet,
        result::Result,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

#[derive(Clone)]
pub struct ConfirmedBlockUploadConfig {
    pub force_reupload: bool,
    pub max_num_slots_to_check: usize,
    pub num_blocks_to_upload_in_parallel: usize,
    pub block_read_ahead_depth: usize, // should always be >= `num_blocks_to_upload_in_parallel`
}

impl Default for ConfirmedBlockUploadConfig {
    fn default() -> Self {
        let num_blocks_to_upload_in_parallel = num_cpus::get() / 2;
        ConfirmedBlockUploadConfig {
            force_reupload: false,
            max_num_slots_to_check: num_blocks_to_upload_in_parallel * 4,
            num_blocks_to_upload_in_parallel,
            block_read_ahead_depth: num_blocks_to_upload_in_parallel * 2,
        }
    }
}

struct BlockstoreLoadStats {
    pub num_blocks_read: usize,
    pub elapsed: Duration,
}

fn maybe_convert_update_parent_to_block_header(
    marker: VersionedBlockMarker,
) -> VersionedBlockMarker {
    let Some(update_parent) = marker.as_update_parent() else {
        return marker;
    };

    // Blockstore omits everything before UpdateParent from the uploaded block,
    // so UpdateParent becomes the effective header in Bigtable.
    VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: update_parent.new_parent_slot,
        parent_block_id: update_parent.new_parent_block_id,
    })
}

fn split_components_for_upload(
    components: Vec<ConfirmedBlockComponent>,
) -> Result<(Vec<EntrySummary>, Vec<VersionedBlockMarker>), &'static str> {
    let mut entries = Vec::new();
    let mut markers = Vec::new();
    let mut seen_parent_marker = false;

    for component in components {
        match component {
            ConfirmedBlockComponent::EntryBatch(entry_batch) => entries.extend(entry_batch),
            ConfirmedBlockComponent::BlockMarker(marker) => {
                if marker.is_parent_marker() {
                    // Rooted data exposes either the original header or the effective
                    // header synthesized from UpdateParent, never both.
                    if seen_parent_marker {
                        return Err("rooted block should only contain one parent marker");
                    }
                    seen_parent_marker = true;
                }

                let marker = maybe_convert_update_parent_to_block_header(marker);
                markers.push(marker);
            }
        }
    }

    Ok((entries, markers))
}

fn get_confirmed_block_upload_data(
    blockstore: &Blockstore,
    slot: Slot,
) -> Result<VersionedConfirmedBlockWithSplitComponents, Box<dyn std::error::Error>> {
    let VersionedConfirmedBlockWithComponents { block, components } =
        blockstore.get_rooted_block_with_components(slot, true)?;
    let (entries, markers) = split_components_for_upload(components)?;

    Ok(VersionedConfirmedBlockWithSplitComponents {
        block,
        entries,
        markers,
    })
}

/// Uploads a range of blocks from a Blockstore to bigtable LedgerStorage
/// Returns the Slot of the last block checked. If no blocks in the range `[staring_slot,
/// ending_slot]` are found in Blockstore, this value is equal to `ending_slot`.
pub async fn upload_confirmed_blocks(
    blockstore: Arc<Blockstore>,
    bigtable: solana_storage_bigtable::LedgerStorage,
    starting_slot: Slot,
    ending_slot: Slot,
    config: ConfirmedBlockUploadConfig,
    exit: Arc<AtomicBool>,
) -> Result<Slot, Box<dyn std::error::Error>> {
    let mut measure = Measure::start("entire upload");

    info!("Loading ledger slots from {starting_slot} to {ending_slot}");
    let blockstore_slots: Vec<_> = blockstore
        .rooted_slot_iterator(starting_slot)
        .map_err(|err| {
            format!("Failed to load entries starting from slot {starting_slot}: {err:?}")
        })?
        .take_while(|slot| *slot <= ending_slot)
        .collect();

    if blockstore_slots.is_empty() {
        warn!("Ledger has no slots from {starting_slot} to {ending_slot:?}");
        return Ok(ending_slot);
    }

    let first_blockstore_slot = *blockstore_slots.first().unwrap();
    let last_blockstore_slot = *blockstore_slots.last().unwrap();
    info!(
        "Found {} slots in the range ({}, {})",
        blockstore_slots.len(),
        first_blockstore_slot,
        last_blockstore_slot,
    );

    // Gather the blocks that are already present in bigtable, by slot
    let bigtable_slots = if !config.force_reupload {
        let mut bigtable_slots = vec![];
        info!(
            "Loading list of bigtable blocks between slots {first_blockstore_slot} and \
             {last_blockstore_slot}..."
        );

        let mut start_slot = first_blockstore_slot;
        while start_slot <= last_blockstore_slot {
            let mut next_bigtable_slots = loop {
                let num_bigtable_blocks = min(1000, config.max_num_slots_to_check * 2);
                match bigtable
                    .get_confirmed_blocks(start_slot, num_bigtable_blocks)
                    .await
                {
                    Ok(slots) => break slots,
                    Err(err) => {
                        error!("get_confirmed_blocks for {start_slot} failed: {err:?}");
                        // Consider exponential backoff...
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            };
            if next_bigtable_slots.is_empty() {
                break;
            }
            bigtable_slots.append(&mut next_bigtable_slots);
            start_slot = bigtable_slots.last().unwrap() + 1;
        }
        bigtable_slots
            .into_iter()
            .filter(|slot| *slot <= last_blockstore_slot)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    // The blocks that still need to be uploaded is the difference between what's already in the
    // bigtable and what's in blockstore...
    let blocks_to_upload = {
        let blockstore_slots = blockstore_slots.into_iter().collect::<HashSet<_>>();
        let bigtable_slots = bigtable_slots.into_iter().collect::<HashSet<_>>();

        let mut blocks_to_upload = blockstore_slots
            .difference(&bigtable_slots)
            .cloned()
            .collect::<Vec<_>>();
        blocks_to_upload.sort_unstable();
        blocks_to_upload.truncate(config.max_num_slots_to_check);
        blocks_to_upload
    };

    if blocks_to_upload.is_empty() {
        info!(
            "No blocks between {starting_slot} and {ending_slot} need to be uploaded to bigtable"
        );
        return Ok(ending_slot);
    }
    let last_slot = *blocks_to_upload.last().unwrap();
    info!(
        "{} blocks to be uploaded to the bucket in the range ({}, {})",
        blocks_to_upload.len(),
        blocks_to_upload.first().unwrap(),
        last_slot
    );

    // Distribute the blockstore reading across a few background threads to speed up the bigtable uploading
    let (loader_threads, receiver): (Vec<_>, _) = {
        let exit = exit.clone();

        let (sender, receiver) = bounded(config.block_read_ahead_depth);

        let (slot_sender, slot_receiver) = unbounded();
        blocks_to_upload
            .into_iter()
            .for_each(|b| slot_sender.send(b).unwrap());
        drop(slot_sender);

        (
            (0..config.num_blocks_to_upload_in_parallel)
                .map(|i| {
                    let blockstore = blockstore.clone();
                    let sender = sender.clone();
                    let slot_receiver = slot_receiver.clone();
                    let exit = exit.clone();
                    std::thread::Builder::new()
                        .name(format!("solBigTGetBlk{i:02}"))
                        .spawn(move || {
                            let start = Instant::now();
                            let mut num_blocks_read = 0;

                            while let Ok(slot) = slot_receiver.recv() {
                                if exit.load(Ordering::Relaxed) {
                                    break;
                                }

                                let _ = match get_confirmed_block_upload_data(&blockstore, slot) {
                                    Ok(upload_data) => {
                                        num_blocks_read += 1;
                                        sender.send((slot, Some(upload_data)))
                                    }
                                    Err(err) => {
                                        error!(
                                            "Failed to get load confirmed block from slot {slot}: \
                                             {err:?}"
                                        );
                                        sender.send((slot, None))
                                    }
                                };
                            }
                            BlockstoreLoadStats {
                                num_blocks_read,
                                elapsed: start.elapsed(),
                            }
                        })
                        .unwrap()
                })
                .collect(),
            receiver,
        )
    };

    let mut failures = 0;
    use futures::stream::StreamExt;

    let mut stream = tokio_stream::iter(receiver).chunks(config.num_blocks_to_upload_in_parallel);

    while let Some(blocks) = stream.next().await {
        if exit.load(Ordering::Relaxed) {
            break;
        }

        let mut measure_upload = Measure::start("Upload");
        let mut num_blocks = blocks.len();
        info!("Preparing the next {num_blocks} blocks for upload");

        let uploads = blocks.into_iter().filter_map(|(slot, block)| match block {
            None => {
                num_blocks -= 1;
                None
            }
            Some(confirmed_block) => {
                let bt = bigtable.clone();
                Some(tokio::spawn(async move {
                    bt.upload_confirmed_block_with_split_components(slot, confirmed_block)
                        .await
                }))
            }
        });

        for result in futures::future::join_all(uploads).await {
            if let Err(err) = result {
                error!("upload_confirmed_block() join failed: {err:?}");
                failures += 1;
            } else if let Err(err) = result.unwrap() {
                error!("upload_confirmed_block() upload failed: {err:?}");
                failures += 1;
            }
        }

        measure_upload.stop();
        info!("{measure_upload} for {num_blocks} blocks");
    }

    measure.stop();
    info!("{measure}");

    let blockstore_results = loader_threads.into_iter().map(|t| t.join());

    let mut blockstore_num_blocks_read = 0;
    let mut blockstore_load_wallclock = Duration::default();
    let mut blockstore_errors = 0;

    for r in blockstore_results {
        match r {
            Ok(stats) => {
                blockstore_num_blocks_read += stats.num_blocks_read;
                blockstore_load_wallclock = max(stats.elapsed, blockstore_load_wallclock);
            }
            Err(e) => {
                error!("error joining blockstore thread: {e:?}");
                blockstore_errors += 1;
            }
        }
    }

    info!(
        "blockstore upload took {:?} for {} blocks ({:.2} blocks/s) errors: {}",
        blockstore_load_wallclock,
        blockstore_num_blocks_read,
        blockstore_num_blocks_read as f64 / blockstore_load_wallclock.as_secs_f64(),
        blockstore_errors
    );

    if failures > 0 {
        Err(format!("Incomplete upload, {failures} operations failed").into())
    } else {
        Ok(last_slot)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_entry::block_component::{BlockFooterV1, UpdateParentV1},
        solana_hash::Hash,
    };

    #[test]
    fn test_split_components_converts_update_parent_to_block_header() {
        let new_parent_slot = 42;
        let new_parent_block_id = Hash::new_unique();
        let first_entry = EntrySummary {
            num_hashes: 1,
            hash: Hash::new_unique(),
            num_transactions: 2,
            starting_transaction_index: 0,
        };
        let second_entry = EntrySummary {
            num_hashes: 3,
            hash: Hash::new_unique(),
            num_transactions: 4,
            starting_transaction_index: 2,
        };
        let update_parent = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
            new_parent_slot,
            new_parent_block_id,
        });
        let footer = VersionedBlockMarker::from_block_footer(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 0,
            block_user_agent: Vec::new(),
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });
        let (entries, markers) = split_components_for_upload(vec![
            ConfirmedBlockComponent::BlockMarker(update_parent),
            ConfirmedBlockComponent::EntryBatch(vec![first_entry]),
            ConfirmedBlockComponent::EntryBatch(vec![second_entry]),
            ConfirmedBlockComponent::BlockMarker(footer.clone()),
        ])
        .unwrap();

        assert_eq!(
            markers,
            vec![
                VersionedBlockMarker::from_block_header(BlockHeaderV1 {
                    parent_slot: new_parent_slot,
                    parent_block_id: new_parent_block_id,
                }),
                footer,
            ]
        );
        assert_eq!(
            entries.iter().map(|entry| entry.hash).collect::<Vec<_>>(),
            vec![first_entry.hash, second_entry.hash]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.starting_transaction_index)
                .collect::<Vec<_>>(),
            vec![
                first_entry.starting_transaction_index,
                second_entry.starting_transaction_index,
            ]
        );
    }
}
