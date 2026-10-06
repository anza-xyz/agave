use {
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender},
    itertools::{Either, Itertools},
    rayon::{ThreadPool, ThreadPoolBuilder, prelude::*},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore_meta::BlockLocation,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            self,
            layout::{is_retransmitter_signed_variant, set_retransmitter_signature},
        },
        sigverify_shreds::{LruCache, SlotPubkeys, par_verify_shreds},
    },
    solana_perf::{
        self,
        deduper::Deduper,
        packet::{BytesPacket, PacketBatch},
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signature::Signature,
    solana_signer::Signer,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        num::NonZeroUsize,
        sync::{Arc, RwLock},
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

// 34MB where each cache entry is 136 bytes.
const SIGVERIFY_LRU_CACHE_CAPACITY: usize = 1 << 18;

const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);

/// Maximum number of packet batches processed in a single sigverify iteration.
///
/// In case of legitimate sigverify traffic sigverify stage keeps up with fetch stage and processes
/// one packet batch per iteration. If a backlog accumulates, this limits each iteration to four
/// batches (at most 256 packets), which takes about 1 ms in observed production workloads.
const SIGVERIFY_SHRED_BATCH_SIZE: usize = 4;

#[allow(clippy::enum_variant_names)]
enum ShredSigverifyError {
    RecvDisconnected,
    RecvTimeout,
    SendError,
}

pub type RepairNonceLocationLookup = dyn Fn(shred::Nonce) -> Option<BlockLocation> + Send + Sync;

pub fn spawn_shred_sigverify(
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    shred_fetch_receiver: Receiver<PacketBatch>,
    retransmit_sender: EvictingSender<Vec<shred::Payload>>,
    verified_sender: Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    repair_nonce_location_lookup: Arc<RepairNonceLocationLookup>,
    num_sigverify_threads: NonZeroUsize,
) -> JoinHandle<()> {
    let mut stats = ShredSigVerifyStats::new(Instant::now());
    let cache = RwLock::new(LruCache::new(SIGVERIFY_LRU_CACHE_CAPACITY));
    let thread_pool = ThreadPoolBuilder::new()
        .num_threads(num_sigverify_threads.get())
        .thread_name(|i| format!("solSvrfyShred{i:02}"))
        .build()
        .expect("new rayon threadpool");
    let run_shred_sigverify = move || {
        let mut rng = rand::rng();
        let deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
        let mut shred_buffer = Vec::with_capacity(SIGVERIFY_SHRED_BATCH_SIZE);
        loop {
            if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, DEDUPER_RESET_CYCLE) {
                stats.num_deduper_saturations += 1;
            }
            // We can't store the keypair outside the loop
            // because the identity might be hot swapped.
            let keypair = cluster_info.keypair();
            match run_shred_sigverify(
                &thread_pool,
                &keypair,
                &bank_forks,
                &leader_schedule_cache,
                &deduper,
                &shred_fetch_receiver,
                &retransmit_sender,
                &verified_sender,
                repair_nonce_location_lookup.as_ref(),
                &cache,
                &mut stats,
                &mut shred_buffer,
            ) {
                Ok(()) => (),
                Err(ShredSigverifyError::RecvTimeout) => (),
                Err(ShredSigverifyError::RecvDisconnected) => break,
                Err(ShredSigverifyError::SendError) => break,
            }
            stats.maybe_submit();
        }
    };
    Builder::new()
        .name("solShredVerifr".to_string())
        .spawn(run_shred_sigverify)
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn run_shred_sigverify<const K: usize>(
    thread_pool: &ThreadPool,
    keypair: &Keypair,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    deduper: &Deduper<K, [u8]>,
    shred_fetch_receiver: &Receiver<PacketBatch>,
    retransmit_sender: &EvictingSender<Vec<shred::Payload>>,
    verified_sender: &Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    repair_nonce_location_lookup: &RepairNonceLocationLookup,
    cache: &RwLock<LruCache>,
    stats: &mut ShredSigVerifyStats,
    shred_buffer: &mut Vec<PacketBatch>,
) -> Result<(), ShredSigverifyError> {
    const RECV_TIMEOUT: Duration = Duration::from_secs(1);
    let packets = shred_fetch_receiver.recv_timeout(RECV_TIMEOUT)?;
    stats.num_packets += packets.len();
    shred_buffer.push(packets);
    for packets in shred_fetch_receiver
        .try_iter()
        .take(SIGVERIFY_SHRED_BATCH_SIZE - 1)
    {
        stats.num_packets += packets.len();
        shred_buffer.push(packets);
    }

    let now = Instant::now();
    stats.num_iters += 1;
    stats.num_batches += shred_buffer.len();
    stats.num_discards_pre += count_discards(shred_buffer);
    // Repair shreds include a randomly generated u32 nonce, so it does not
    // make sense to deduplicate the entire packet payload (i.e. they are not
    // duplicate of any other packet.data(..)).
    // If the nonce is excluded from the deduper then false positives might
    // prevent us from repairing a block until the deduper is reset after
    // DEDUPER_RESET_CYCLE. A workaround is to also repair "coding" shreds to
    // add some redundancy but that is not implemented at the moment.
    // Because the repair nonce is already verified in shred-fetch-stage we can
    // exclude repair shreds from the deduper, but we still need to pass the
    // repair shred to the deduper to filter out duplicates from the turbine
    // path once a shred is repaired.
    // For backward compatibility we need to allow trailing bytes in the packet
    // after the shred payload, but have to exclude them here from the deduper.
    stats.num_duplicates += thread_pool.install(|| {
        shred_buffer
            .par_iter_mut()
            .flatten()
            .filter(|packet| {
                !packet.meta().discard()
                    && shred::wire::get_shred(packet)
                        .map(|shred| deduper.dedup(shred))
                        .unwrap_or(true)
                    && !packet.meta().repair()
            })
            .map(|packet| packet.meta_mut().set_discard(true))
            .count()
    });
    let working_bank = bank_forks.read().unwrap().working_bank();
    thread_pool.install(|| {
        par_verify_packets(
            &keypair.pubkey(),
            &working_bank,
            leader_schedule_cache,
            shred_buffer,
            cache,
        )
    });
    stats.num_discards_post += count_discards(shred_buffer);
    // Extract shred payload from packets, and separate out repaired shreds.
    let (shreds, repairs): (Vec<_>, Vec<_>) = shred_buffer
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
        .filter_map(|packet| {
            extract_shred_and_location(packet, repair_nonce_location_lookup, stats)
        })
        .filter_map(|(mut shred, location)| {
            maybe_clear_retransmitter_signature(&mut shred).ok()?;
            Some((shred, location))
        })
        .partition_map(|(shred, location)| {
            if let Some(location) = location {
                // No need for Arc overhead here because repaired shreds are
                // not retranmitted.
                Either::Right((
                    shred::Payload::from(shred),
                    /* is_repaired */ true,
                    location,
                ))
            } else {
                // Share the payload between the retransmit-stage and the
                // window-service.
                Either::Left(shred::Payload::from(shred))
            }
        });

    // Repaired shreds are not retransmitted.
    stats.num_retransmit_shreds += shreds.len();
    if let Err(send_err) = retransmit_sender.try_send(shreds.clone()) {
        match send_err {
            crossbeam_channel::TrySendError::Full(v) => {
                stats.num_retransmit_stage_overflow_shreds += v.len();
            }
            _ => unreachable!("EvictingSender holds on to both ends of the channel"),
        }
    }
    // Send all shreds to window service to be inserted into blockstore.
    let shreds = shreds
        .into_iter()
        .map(|shred| (shred, /*is_repaired:*/ false, BlockLocation::Original));
    verified_sender.send(shreds.chain(repairs).collect())?;
    stats.elapsed_micros += now.elapsed().as_micros() as u64;
    shred_buffer.clear();
    Ok(())
}

/// Extracts shred bytes and, for repaired shreds, the location where the shred
/// should be inserted into blockstore.
fn extract_shred_and_location(
    packet: &BytesPacket,
    repair_nonce_location_lookup: &RepairNonceLocationLookup,
    stats: &mut ShredSigVerifyStats,
) -> Option<(Vec<u8>, Option<BlockLocation>)> {
    let (shred, nonce) = shred::layout::get_shred_and_repair_nonce(packet)?;
    let Some(nonce) = nonce else {
        // Turbine shred.
        return Some((shred.to_vec(), None));
    };

    // Repair shred.
    if let Some(location) = repair_nonce_location_lookup(nonce) {
        Some((shred.to_vec(), Some(location)))
    } else {
        // This indicates the request entry was evicted before consumption.
        stats.num_unknown_block_location += 1;
        None
    }
}

/// Zeroes the retransmitter signature of the `shred` if it is of resigned variant.
///
/// `verify_retransmitter_signature` feature will never be activated, so we do not need to make
/// them. The signature is outside of the Merkle tree and the leader signed data, so its
/// contents do not affect shred validity. Writing zeros is ensuring all offsets are valid.
///
/// Overwriting the signature with zeros ensures we do not forward untrusted bytes.
fn maybe_clear_retransmitter_signature(shred: &mut [u8]) -> Result<(), shred::Error> {
    if is_retransmitter_signed_variant(shred)? {
        set_retransmitter_signature(shred, &Signature::default())?;
    }
    Ok(())
}

fn par_verify_packets(
    self_pubkey: &Pubkey,
    working_bank: &Bank,
    leader_schedule_cache: &LeaderScheduleCache,
    packets: &mut [PacketBatch],
    cache: &RwLock<LruCache>,
) {
    let leader_slots: SlotPubkeys =
        get_slot_leaders(self_pubkey, packets, leader_schedule_cache, working_bank)
            .filter_map(|(slot, pubkey)| Some((slot, pubkey?)))
            .chain(std::iter::once((Slot::MAX, Pubkey::default())))
            .collect();
    par_verify_shreds(packets, &leader_slots, cache);
}

// Returns pubkey of leaders for shred slots referenced in the packets.
// Marks packets as discard if:
//   - fails to deserialize the shred slot.
//   - slot leader is unknown.
//   - slot leader is the node itself (circular transmission).
fn get_slot_leaders<'a>(
    self_pubkey: &'a Pubkey,
    batches: &'a mut [PacketBatch],
    leader_schedule_cache: &'a LeaderScheduleCache,
    bank: &'a Bank,
) -> impl Iterator<Item = (Slot, Option<Pubkey>)> + 'a {
    batches
        .iter_mut()
        .flat_map(|batch| batch.iter_mut())
        .filter(|packet| !packet.meta().discard())
        .filter_map(move |packet| {
            let shred = shred::layout::get_shred(packet);
            let slot = shred.and_then(shred::layout::get_slot)?;
            let leader = leader_schedule_cache
                .slot_leader_at(slot, Some(bank))
                .map(|leader| leader.id)
                .filter(|leader| leader != self_pubkey);
            if leader.is_none() {
                packet.meta_mut().set_discard(true);
            }
            Some((slot, leader))
        })
}

fn count_discards(packets: &[PacketBatch]) -> usize {
    packets
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| packet.meta().discard())
        .count()
}

impl From<RecvTimeoutError> for ShredSigverifyError {
    fn from(err: RecvTimeoutError) -> Self {
        match err {
            RecvTimeoutError::Timeout => Self::RecvTimeout,
            RecvTimeoutError::Disconnected => Self::RecvDisconnected,
        }
    }
}

impl<T> From<SendError<T>> for ShredSigverifyError {
    fn from(_: SendError<T>) -> Self {
        Self::SendError
    }
}

struct ShredSigVerifyStats {
    since: Instant,
    num_iters: usize,
    num_batches: usize,
    num_packets: usize,
    num_deduper_saturations: usize,
    num_discards_post: usize,
    num_discards_pre: usize,
    num_duplicates: usize,
    num_retransmit_stage_overflow_shreds: usize,
    num_retransmit_shreds: usize,
    /// This means the OutstandingRequests cache is saturated and we
    /// threw away a verified shred due to being unable to fetch the storage location
    num_unknown_block_location: usize,
    elapsed_micros: u64,
}

impl ShredSigVerifyStats {
    const METRICS_SUBMIT_CADENCE: Duration = Duration::from_secs(2);

    fn new(now: Instant) -> Self {
        Self {
            since: now,
            num_iters: 0usize,
            num_batches: 0usize,
            num_packets: 0usize,
            num_discards_pre: 0usize,
            num_deduper_saturations: 0usize,
            num_discards_post: 0usize,
            num_duplicates: 0usize,
            num_retransmit_stage_overflow_shreds: 0usize,
            num_retransmit_shreds: 0usize,
            num_unknown_block_location: 0usize,
            elapsed_micros: 0u64,
        }
    }

    fn maybe_submit(&mut self) {
        if self.since.elapsed() <= Self::METRICS_SUBMIT_CADENCE {
            return;
        }
        datapoint_info!(
            "shred_sigverify",
            ("num_iters", self.num_iters, i64),
            ("num_batches", self.num_batches, i64),
            ("num_packets", self.num_packets, i64),
            ("num_discards_pre", self.num_discards_pre, i64),
            ("num_deduper_saturations", self.num_deduper_saturations, i64),
            ("num_discards_post", self.num_discards_post, i64),
            ("num_duplicates", self.num_duplicates, i64),
            (
                "num_retransmit_stage_overflow_shreds",
                self.num_retransmit_stage_overflow_shreds,
                i64
            ),
            ("num_retransmit_shreds", self.num_retransmit_shreds, i64),
            (
                "num_unknown_block_location",
                self.num_unknown_block_location,
                i64
            ),
            ("elapsed_micros", self.elapsed_micros, i64),
        );
        *self = Self::new(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::Rng,
        solana_entry::entry::{Entry, create_ticks},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            genesis_utils::create_genesis_config_with_leader,
            shred::{ProcessShredsStats, Shredder, layout::get_retransmitter_signature},
        },
        solana_perf::packet::BytesPacketBatch,
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        test_case::test_case,
    };

    #[test]
    fn test_sigverify_shreds_verify_batches() {
        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let leader_pubkey = leader_keypair.pubkey();
        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let bank_forks = BankForks::new_rw_arc(bank);
        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let (shreds_data, _shreds_code) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );
        let (shreds_data_wrong, _shreds_code_wrong) = shredder.entries_to_merkle_shreds_for_tests(
            &wrong_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );

        let batch = BytesPacketBatch::from(vec![
            shreds_data[0].payload().to_bytes_packet(None),
            shreds_data_wrong[0].payload().to_bytes_packet(None),
        ]);
        let batches = vec![batch];

        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let working_bank = bank_forks.read().unwrap().working_bank();
        let mut batches = batches
            .into_iter()
            .map(PacketBatch::from)
            .collect::<Vec<_>>();
        thread_pool.install(|| {
            par_verify_packets(
                &Pubkey::new_unique(), // self_pubkey
                &working_bank,
                &leader_schedule_cache,
                &mut batches,
                &cache,
            )
        });
        assert!(!batches[0].first().unwrap().meta().discard());
        assert!(batches[0].get(1).unwrap().meta().discard());
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_maybe_clear_retransmitter_signature(is_last_in_slot: bool) {
        let mut rng = rand::rng();

        let leader_keypair = Keypair::new();
        let chained_merkle_root = Hash::new_from_array(rng.random());

        let shredder = Shredder::new(0, 0, 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        // Only older leaders resign shreds.
        let (data_shreds, coding_shreds) = shredder.entries_to_resigned_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            0,
            0,
        );
        let shreds: Vec<_> = data_shreds.into_iter().chain(coding_shreds).collect();

        let upstream_signature = Signature::from([1u8; 64]);
        for shred in shreds {
            let mut payload = shred.payload().to_vec();
            if is_last_in_slot {
                set_retransmitter_signature(&mut payload, &upstream_signature)
                    .expect("last FEC set in slot should be of resigned variant");
            }
            let original = payload.clone();
            maybe_clear_retransmitter_signature(&mut payload)
                .expect("valid shred should not be rejected");

            if is_last_in_slot {
                assert_eq!(
                    get_retransmitter_signature(&payload)
                        .expect("resigned variant should have retransmitter signature"),
                    Signature::default(),
                    "retransmitter signature should be zeroed"
                );
            } else {
                assert_eq!(payload, original, "shred should not be modified");
            }
        }
    }
}
