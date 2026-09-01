#![allow(clippy::arithmetic_side_effects)]

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

use {
    crossbeam_channel::bounded,
    solana_entry::entry::create_ticks,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        genesis_utils::create_genesis_config_with_leader,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            DATA_SHREDS_PER_FEC_BLOCK, ProcessShredsStats, ReedSolomonCache, Shredder,
            get_data_shred_bytes_per_batch_typical, max_ticks_per_n_shreds,
        },
    },
    solana_net_utils::SocketAddrSpace,
    solana_perf::packet::{PACKETS_PER_BATCH, Packet, PacketBatch, RecycledPacketBatch},
    solana_rayon_threadlimit::get_thread_count,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    solana_streamer::evicting_sender::EvictingSender,
    solana_time_utils::timestamp,
    solana_turbine::sigverify_shreds::{RepairNonceLocationLookup, spawn_shred_sigverify},
    std::{
        env,
        num::NonZeroUsize,
        sync::Arc,
        thread,
        time::{Duration, Instant},
    },
};

const SLOT_DURATION: Duration = Duration::from_millis(400);

const CHANNEL_CAPACITY: usize = 1_024;

const DEFAULT_NUM_SLOTS: usize = 50;
const DEFAULT_BATCHES_PER_SLOT: usize = 500;
const DEFAULT_PACKETS_PER_BATCH: usize = PACKETS_PER_BATCH;
const DEFAULT_INVALID_PACKETS_PER_SLOT: usize = 0;

const FIRST_SLOT: u64 = 1;

#[derive(Clone, Copy, Debug)]
enum ReplayMode {
    Paced,
    Saturate,
}

fn replay_mode() -> ReplayMode {
    match env::var("SHRED_SIGVERIFY_MODE").as_deref() {
        Ok("paced") | Err(_) => ReplayMode::Paced,
        Ok("saturate") => ReplayMode::Saturate,
        Ok(mode) => {
            panic!("SHRED_SIGVERIFY_MODE must be either 'paced' or 'saturate', got '{mode}'")
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{name} must be an unsigned integer")),
        Err(_) => default,
    }
}

fn shred_size_typical() -> usize {
    let batch_payload = get_data_shred_bytes_per_batch_typical() as usize;
    batch_payload / DATA_SHREDS_PER_FEC_BLOCK
}

fn should_corrupt_packet(
    packet_index: usize,
    packets_per_slot: usize,
    invalid_packets_per_slot: usize,
) -> bool {
    if invalid_packets_per_slot == 0 {
        return false;
    }

    packet_index * invalid_packets_per_slot / packets_per_slot
        != (packet_index + 1) * invalid_packets_per_slot / packets_per_slot
}

fn corrupt_shred_signature(packet: &mut Packet) {
    // The leader signature starts at the beginning of the shred payload.
    // Flipping one signature byte keeps the shred layout intact while making
    // signature verification fail.
    packet.buffer_mut()[0] ^= 1;
}

fn make_slot_batches(
    leader_keypair: &Keypair,
    slot: u64,
    batches_per_slot: usize,
    packets_per_batch: usize,
    invalid_packets_per_slot: usize,
) -> Vec<PacketBatch> {
    let shreds_per_slot = batches_per_slot * packets_per_batch;
    let shred_size = shred_size_typical();

    let ticks_per_shred = max_ticks_per_n_shreds(1, Some(shred_size)).max(1);
    let num_ticks = ticks_per_shred * shreds_per_slot as u64;

    let entries = create_ticks(num_ticks, 0, Hash::new_unique());
    let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 0).unwrap();

    // Use variants without retransmitter signatures so the workload primarily
    // measures leader signature verification.
    let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
        leader_keypair,
        &entries,
        false,
        Hash::new_unique(),
        0,
        0,
        &ReedSolomonCache::default(),
        &mut ProcessShredsStats::default(),
    );

    let mut shreds = data_shreds;
    shreds.extend(coding_shreds);

    assert!(
        shreds.len() >= shreds_per_slot,
        "shred generator produced {} shreds, expected at least {}",
        shreds.len(),
        shreds_per_slot,
    );

    shreds.truncate(shreds_per_slot);

    let mut batches = Vec::with_capacity(batches_per_slot);
    let mut corrupted_packets = 0usize;

    for (batch_index, shreds) in shreds.chunks(packets_per_batch).enumerate() {
        let mut batch = RecycledPacketBatch::with_capacity(shreds.len());

        for (packet_index, shred) in shreds.iter().enumerate() {
            let slot_packet_index = batch_index * packets_per_batch + packet_index;
            let mut packet = shred.payload().to_packet(None);

            if should_corrupt_packet(slot_packet_index, shreds_per_slot, invalid_packets_per_slot) {
                corrupt_shred_signature(&mut packet);
                corrupted_packets += 1;
            }

            batch.push(packet);
        }

        batches.push(PacketBatch::from(batch));
    }

    assert_eq!(batches.len(), batches_per_slot);
    assert_eq!(corrupted_packets, invalid_packets_per_slot);

    batches
}

fn make_workload(
    leader_keypair: &Keypair,
    num_slots: usize,
    batches_per_slot: usize,
    packets_per_batch: usize,
    invalid_packets_per_slot: usize,
) -> Vec<PacketBatch> {
    let mut workload = Vec::with_capacity(num_slots * batches_per_slot);

    for slot_offset in 0..num_slots {
        let slot = FIRST_SLOT + slot_offset as u64;

        workload.extend(make_slot_batches(
            leader_keypair,
            slot,
            batches_per_slot,
            packets_per_batch,
            invalid_packets_per_slot,
        ));
    }

    workload
}

/// Returns the target arrival time for `num_shreds` from the start of replay.
///
/// The replay rate is derived from the number of shreds per 400 ms slot.
fn arrival_offset(num_shreds: usize, shreds_per_slot: usize) -> Duration {
    let nanos = num_shreds as u128 * SLOT_DURATION.as_nanos() / shreds_per_slot as u128;

    Duration::from_nanos(nanos as u64)
}

fn sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();

        if now >= deadline {
            return;
        }

        thread::sleep(deadline - now);
    }
}

fn main() {
    let replay_mode = replay_mode();

    let num_slots = env_usize("SHRED_SIGVERIFY_SLOTS", DEFAULT_NUM_SLOTS);

    let batches_per_slot = env_usize("SHRED_SIGVERIFY_BATCHES_PER_SLOT", DEFAULT_BATCHES_PER_SLOT);

    let packets_per_batch = env_usize(
        "SHRED_SIGVERIFY_PACKETS_PER_BATCH",
        DEFAULT_PACKETS_PER_BATCH,
    );

    let invalid_packets_per_slot = env_usize(
        "SHRED_SIGVERIFY_INVALID_PACKETS_PER_SLOT",
        DEFAULT_INVALID_PACKETS_PER_SLOT,
    );

    let num_sigverify_threads =
        NonZeroUsize::new(env_usize("SHRED_SIGVERIFY_THREADS", get_thread_count()))
            .expect("SHRED_SIGVERIFY_THREADS must be greater than zero");

    assert!(
        num_slots > 0,
        "SHRED_SIGVERIFY_SLOTS must be greater than zero"
    );

    assert!(
        batches_per_slot > 0,
        "SHRED_SIGVERIFY_BATCHES_PER_SLOT must be greater than zero"
    );

    assert!(
        packets_per_batch > 0,
        "SHRED_SIGVERIFY_PACKETS_PER_BATCH must be greater than zero"
    );

    assert!(
        packets_per_batch <= PACKETS_PER_BATCH,
        "SHRED_SIGVERIFY_PACKETS_PER_BATCH must not exceed {PACKETS_PER_BATCH}"
    );

    let shreds_per_slot = batches_per_slot * packets_per_batch;

    assert!(
        invalid_packets_per_slot <= shreds_per_slot,
        "SHRED_SIGVERIFY_INVALID_PACKETS_PER_SLOT must not exceed packets per slot"
    );

    let expected_shreds = num_slots * shreds_per_slot;
    let expected_invalid_shreds = num_slots * invalid_packets_per_slot;
    let expected_valid_shreds = expected_shreds - expected_invalid_shreds;

    println!(
        "shred sigverify workload: mode={replay_mode:?}, slots={num_slots}, \
         batches_per_slot={batches_per_slot}, packets_per_batch={packets_per_batch}, \
         shreds_per_slot={shreds_per_slot}, invalid_packets_per_slot={invalid_packets_per_slot}, \
         total_shreds={expected_shreds}, intentionally_invalid={expected_invalid_shreds}, \
         sigverify_threads={num_sigverify_threads}"
    );

    let leader_keypair = Arc::new(Keypair::new());
    let leader_pubkey = leader_keypair.pubkey();

    // The validator running sigverify must not be the leader.
    let node_keypair = Arc::new(Keypair::new());
    let node_pubkey = node_keypair.pubkey();

    let workload = make_workload(
        leader_keypair.as_ref(),
        num_slots,
        batches_per_slot,
        packets_per_batch,
        invalid_packets_per_slot,
    );

    println!(
        "Shred sigverify workload ready; attach profiler to pid {}",
        std::process::id()
    );

    assert_eq!(workload.len(), num_slots * batches_per_slot);

    assert_eq!(
        workload.iter().map(PacketBatch::len).sum::<usize>(),
        expected_shreds,
    );

    let bank = Bank::new_for_tests(
        &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
    );

    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
    let bank_forks = BankForks::new_rw_arc(bank);

    let cluster_info = Arc::new(ClusterInfo::new(
        ContactInfo::new_localhost(&node_pubkey, timestamp()),
        node_keypair,
        SocketAddrSpace::Unspecified,
    ));

    let (shred_fetch_sender, shred_fetch_receiver) = bounded::<PacketBatch>(CHANNEL_CAPACITY);

    // Retransmit consumption itself is outside this harness.
    let (retransmit_sender, _retransmit_receiver) = EvictingSender::new_bounded(1);

    // Drain verified output continuously so sigverify cannot block on the
    // downstream channel.
    let (verified_sender, verified_receiver) = bounded::<
        Vec<(
            solana_ledger::shred::Payload,
            bool,
            solana_ledger::blockstore_meta::BlockLocation,
        )>,
    >(CHANNEL_CAPACITY);

    let verified_handle = thread::spawn(move || {
        verified_receiver
            .into_iter()
            .map(|shreds| shreds.len())
            .sum::<usize>()
    });

    let repair_nonce_location_lookup: Arc<RepairNonceLocationLookup> = Arc::new(|_| None);

    // Worker creation is outside the measured section.
    let sigverify_handle = spawn_shred_sigverify(
        cluster_info,
        bank_forks,
        leader_schedule_cache,
        shred_fetch_receiver,
        retransmit_sender,
        verified_sender,
        repair_nonce_location_lookup,
        num_sigverify_threads,
    );

    let replay_start = Instant::now();
    let mut sent_shreds = 0usize;
    let mut max_input_queue_depth = 0usize;

    for batch in workload {
        let batch_len = batch.len();

        match replay_mode {
            ReplayMode::Paced => {
                let deadline = replay_start + arrival_offset(sent_shreds, shreds_per_slot);

                sleep_until(deadline);

                shred_fetch_sender
                    .try_send(batch)
                    .expect("shred sigverify input channel must not be full");

                max_input_queue_depth = max_input_queue_depth.max(shred_fetch_sender.len());
            }
            ReplayMode::Saturate => {
                shred_fetch_sender
                    .send(batch)
                    .expect("shred sigverify receiver disconnected");
            }
        }

        sent_shreds += batch_len;
    }

    assert_eq!(sent_shreds, expected_shreds);

    // Closing ingress lets sigverify drain all queued batches and terminate.
    drop(shred_fetch_sender);

    sigverify_handle
        .join()
        .expect("shred sigverify thread panicked");

    let verified_shreds = verified_handle
        .join()
        .expect("verified shred consumer panicked");

    assert!(
        verified_shreds <= expected_valid_shreds,
        "sigverify emitted {verified_shreds} shreds, but only {expected_valid_shreds} input \
         shreds had valid leader signatures"
    );

    let additional_discards = expected_valid_shreds - verified_shreds;
    let replay_elapsed = replay_start.elapsed();
    let expected_replay = SLOT_DURATION * num_slots as u32;

    match replay_mode {
        ReplayMode::Paced => {
            println!(
                "shred sigverify result: mode={replay_mode:?}, sent={sent_shreds}, \
                 intentionally_invalid={expected_invalid_shreds}, \
                 expected_valid={expected_valid_shreds}, verified={verified_shreds}, \
                 additional_discards={additional_discards}, \
                 max_input_queue_depth={max_input_queue_depth}, \
                 replay_elapsed={replay_elapsed:?}, expected_replay={expected_replay:?}"
            );
        }
        ReplayMode::Saturate => {
            println!(
                "shred sigverify result: mode={replay_mode:?}, sent={sent_shreds}, \
                 intentionally_invalid={expected_invalid_shreds}, \
                 expected_valid={expected_valid_shreds}, verified={verified_shreds}, \
                 additional_discards={additional_discards}, replay_elapsed={replay_elapsed:?}, \
                 expected_replay={expected_replay:?}"
            );
        }
    }
}
