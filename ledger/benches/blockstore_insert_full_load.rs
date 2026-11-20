//! Sustained full-load benchmark of the window service insert path.
//!
//! Every slot carries the maximum number of FEC sets. Shreds arrive over the
//! slot as a leader would broadcast them, with turbine jitter and loss, and are
//! inserted into an on-disk blockstore by a single thread, as `run_insert` in
//! window service does. The report shows whether insertion keeps up with the
//! slot rate or falls behind, and how long into an insert call recovered
//! shreds are sent for retransmit.
//!
//! Only uses the public insert API, so it runs unchanged on master and on
//! branches which change the insert path.
//!
//! Configuration through environment variables:
//! - `BENCH_SLOTS`: number of slots to insert (12)
//! - `BENCH_WARMUP_SLOTS`: slots left out of the report (2)
//! - `BENCH_FEC_SETS`: FEC sets per slot (512, the 16384 data shred limit of 200ms slots)
//! - `BENCH_SLOT_MS`: slot duration, i.e. 1000 / blocks per second (200)
//! - `BENCH_JITTER_MS`: max turbine delay of a shred after its FEC set is broadcast (50)
//! - `BENCH_LOSS`: probability of losing a shred (0.02)
//! - `BENCH_BATCH_US`: how often shreds are handed to the insert thread (500)
//! - `BENCH_LEDGER_DIR`: parent directory of the blockstore (`farf` in the workspace)
//!
//! Run with:
//! `cargo bench -p solana-ledger --bench blockstore_insert_full_load`
#![allow(clippy::arithmetic_side_effects)]
use {
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded},
    rand::Rng,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::Blockstore,
        blockstore_metrics::BlockstoreInsertionMetrics,
        genesis_utils::create_genesis_config,
        get_tmp_ledger_path_auto_delete,
        shred::{
            DATA_SHREDS_PER_FEC_BLOCK, MAX_DATA_SHREDS_PER_SLOT, ProcessShredsStats, Shred,
            Shredder, filter::ShredRecoveryContext,
        },
    },
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::{
        borrow::Cow,
        env,
        str::FromStr,
        sync::Arc,
        thread,
        time::{Duration, Instant},
    },
    tempfile::TempDir,
};

const SHRED_VERSION: u16 = 1;
const TXS_PER_ENTRY: usize = 64;
// Data shred limit of 200ms slots, see `SLOT_PARAMS_200MS` in runtime.
const FEC_SETS_PER_200MS_SLOT: usize = 16_384 / DATA_SHREDS_PER_FEC_BLOCK;

struct Config {
    num_slots: u64,
    warmup_slots: u64,
    fec_sets_per_slot: usize,
    slot_duration: Duration,
    max_jitter: Duration,
    loss: f64,
    batch_interval: Duration,
}

impl Config {
    fn from_env() -> Self {
        let max_fec_sets = MAX_DATA_SHREDS_PER_SLOT / DATA_SHREDS_PER_FEC_BLOCK;
        let config = Self {
            num_slots: env_or("BENCH_SLOTS", 12),
            warmup_slots: env_or("BENCH_WARMUP_SLOTS", 2),
            fec_sets_per_slot: env_or("BENCH_FEC_SETS", FEC_SETS_PER_200MS_SLOT),
            slot_duration: Duration::from_millis(env_or("BENCH_SLOT_MS", 200)),
            max_jitter: Duration::from_millis(env_or("BENCH_JITTER_MS", 50)),
            loss: env_or("BENCH_LOSS", 0.02),
            batch_interval: Duration::from_micros(env_or("BENCH_BATCH_US", 500)),
        };
        assert!(
            config.warmup_slots < config.num_slots,
            "BENCH_WARMUP_SLOTS must be less than BENCH_SLOTS"
        );
        assert!(
            config.fec_sets_per_slot <= max_fec_sets,
            "BENCH_FEC_SETS must not exceed {max_fec_sets}"
        );
        config
    }
}

fn env_or<T: FromStr>(name: &str, default: T) -> T {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} should parse")),
        Err(_) => default,
    }
}

fn make_entry() -> Entry {
    // Blockstore does not look into the entries, so one transaction cloned
    // over and over is as good as distinct ones.
    let lamports = 1;
    let transaction = solana_system_transaction::transfer(
        &Keypair::new(),
        &Pubkey::new_unique(),
        lamports,
        Hash::new_unique(),
    );
    let num_hashes = 1;
    Entry::new(
        &Hash::new_unique(),
        num_hashes,
        vec![transaction; TXS_PER_ENTRY],
    )
}

fn make_shreds(
    keypair: &Keypair,
    slot: u64,
    entries: &[Entry],
    chained_merkle_root: Hash,
) -> (Vec<Shred>, Vec<Shred>) {
    let parent_slot = slot - 1;
    let reference_tick = 0;
    let shredder = Shredder::new(slot, parent_slot, reference_tick, SHRED_VERSION)
        .expect("shredder should accept the slot");
    let is_last_in_slot = true;
    let next_shred_index = 0;
    let next_code_index = 0;
    shredder.entries_to_merkle_shreds_for_tests(
        keypair,
        entries,
        is_last_in_slot,
        chained_merkle_root,
        next_shred_index,
        next_code_index,
        &mut ProcessShredsStats::default(),
    )
}

/// Returns the most entries which shred into at most `num_fec_sets` FEC sets.
fn make_slot_entries(keypair: &Keypair, num_fec_sets: usize) -> Vec<Entry> {
    let target_data_shreds = num_fec_sets * DATA_SHREDS_PER_FEC_BLOCK;
    let entry = make_entry();
    let num_data_shreds = |num_entries| {
        let (data, _) = make_shreds(
            keypair,
            1,
            &vec![entry.clone(); num_entries],
            Hash::default(),
        );
        data.len()
    };
    // Binary search for the largest number of entries which fits.
    let (mut fits, mut too_many) = (1, 2);
    while num_data_shreds(too_many) <= target_data_shreds {
        fits = too_many;
        too_many *= 2;
    }
    while too_many - fits > 1 {
        let mid = (fits + too_many) / 2;
        if num_data_shreds(mid) <= target_data_shreds {
            fits = mid;
        } else {
            too_many = mid;
        }
    }
    vec![entry; fits]
}

struct Arrival {
    offset: Duration,
    shred: Shred,
}

struct Schedule {
    arrivals: Vec<Arrival>,
    /// Offset at which the last FEC set of each slot is broadcast, indexed by slot - 1.
    slot_last_broadcast: Vec<Duration>,
    num_fec_sets_per_slot: usize,
    num_shreds_per_slot: usize,
}

/// Generates the shreds of all slots and the times they reach the insert
/// thread, relative to the start of the first slot.
fn make_schedule(config: &Config) -> Schedule {
    let mut rng = rand::rng();
    let keypair = Keypair::new();
    let entries = make_slot_entries(&keypair, config.fec_sets_per_slot);
    let mut arrivals = Vec::new();
    let mut slot_last_broadcast = Vec::new();
    let mut chained_merkle_root = Hash::new_unique();
    let mut num_fec_sets_per_slot = 0;
    let mut num_shreds_per_slot = 0;
    for slot in 1..=config.num_slots {
        let (data, coding) = make_shreds(&keypair, slot, &entries, chained_merkle_root);
        chained_merkle_root = data
            .last()
            .expect("slot should have data shreds")
            .merkle_root()
            .expect("merkle shreds should have a merkle root");
        num_fec_sets_per_slot = data.len() / DATA_SHREDS_PER_FEC_BLOCK;
        let slot_start = config.slot_duration * (slot - 1) as u32;
        let fec_set_interval = config.slot_duration / num_fec_sets_per_slot as u32;
        for (fec_set, (data_shreds, coding_shreds)) in data
            .chunks(DATA_SHREDS_PER_FEC_BLOCK)
            .zip(coding.chunks(DATA_SHREDS_PER_FEC_BLOCK))
            .enumerate()
        {
            let broadcast = slot_start + fec_set_interval * (fec_set + 1) as u32;
            if fec_set + 1 == num_fec_sets_per_slot {
                slot_last_broadcast.push(broadcast);
            }
            let mut num_kept = 0;
            let num_shreds = data_shreds.len() + coding_shreds.len();
            for (i, shred) in data_shreds.iter().chain(coding_shreds).enumerate() {
                // There is no repair, so keep enough shreds to recover the set.
                let num_left = num_shreds - i;
                let must_keep = num_kept + num_left <= DATA_SHREDS_PER_FEC_BLOCK;
                if !must_keep && rng.random_bool(config.loss) {
                    continue;
                }
                num_kept += 1;
                let offset = broadcast + config.max_jitter.mul_f64(rng.random());
                arrivals.push(Arrival {
                    offset,
                    shred: shred.clone(),
                });
            }
            num_shreds_per_slot += num_kept;
        }
    }
    arrivals.sort_unstable_by_key(|arrival| arrival.offset);
    Schedule {
        arrivals,
        slot_last_broadcast,
        num_fec_sets_per_slot,
        num_shreds_per_slot: num_shreds_per_slot / config.num_slots as usize,
    }
}

/// Hands shreds over to the insert thread at their arrival times, in batches
/// like those shred sigverify sends to window service.
fn run_feeder(
    arrivals: Vec<Arrival>,
    batch_interval: Duration,
    start: Instant,
    sender: Sender<(Instant, Vec<Shred>)>,
) {
    let mut arrivals = arrivals.into_iter().peekable();
    while let Some(next) = arrivals.peek() {
        let now = start.elapsed();
        if next.offset > now {
            thread::sleep((next.offset - now).max(batch_interval));
            continue;
        }
        let now = start.elapsed();
        let mut batch = Vec::new();
        while let Some(arrival) = arrivals.next_if(|arrival| arrival.offset <= now) {
            batch.push(arrival.shred);
        }
        if sender.send((Instant::now(), batch)).is_err() {
            return;
        }
        thread::sleep(batch_interval);
    }
}

fn percentile(sorted: &[Duration], percentile: usize) -> Duration {
    sorted
        .get((sorted.len().saturating_sub(1)) * percentile / 100)
        .copied()
        .unwrap_or_default()
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn main() {
    let config = Config::from_env();
    println!(
        "generating {} slots of {} FEC sets",
        config.num_slots, config.fec_sets_per_slot
    );
    let schedule = make_schedule(&config);
    let total_shreds = schedule.arrivals.len();

    let ledger_dir = match env::var("BENCH_LEDGER_DIR") {
        Ok(dir) => TempDir::new_in(dir).expect("BENCH_LEDGER_DIR should be writable"),
        Err(_) => get_tmp_ledger_path_auto_delete!(),
    };
    let blockstore = Blockstore::open(ledger_dir.path()).expect("blockstore should open");
    let genesis_config = create_genesis_config(1).genesis_config;
    let root_bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let (retransmit_sender, retransmit_receiver) = bounded(1024);
    let mut shred_recovery_context =
        ShredRecoveryContext::new(retransmit_sender, root_bank, SHRED_VERSION);
    let mut metrics = BlockstoreInsertionMetrics::default();

    println!(
        "inserting {} slots of {} FEC sets ({} shreds after loss) into {:?}, slot duration {:?}, \
         jitter up to {:?}, loss {}, batch interval {:?}",
        config.num_slots,
        schedule.num_fec_sets_per_slot,
        schedule.num_shreds_per_slot,
        ledger_dir.path(),
        config.slot_duration,
        config.max_jitter,
        config.loss,
        config.batch_interval,
    );
    // Recovered shreds are sent for retransmit from inside the insert call.
    // Timestamp them on arrival to measure how long into the call that is.
    let retransmitter = thread::spawn(move || {
        let mut received = Vec::new();
        while let Ok(shreds) = retransmit_receiver.recv() {
            received.push((Instant::now(), shreds.len()));
        }
        received
    });
    let (sender, receiver): (_, Receiver<(Instant, Vec<Shred>)>) = unbounded();
    let start = Instant::now();
    let feeder = {
        let batch_interval = config.batch_interval;
        let arrivals = schedule.arrivals;
        thread::spawn(move || run_feeder(arrivals, batch_interval, start, sender))
    };

    let measure_from = config.slot_duration * config.warmup_slots as u32;
    let mut slot_full_at = vec![None; config.num_slots as usize];
    let mut next_pending_slot = 1;
    let mut num_received = 0;
    let mut busy = Duration::ZERO;
    let mut queue_delays = Vec::new();
    let mut batch_sizes = Vec::new();
    let mut insert_times = Vec::new();
    let mut insert_starts = Vec::new();
    while next_pending_slot <= config.num_slots {
        let mut batches = match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(batch) => vec![batch],
            Err(RecvTimeoutError::Timeout) => {
                println!("no shreds for 5s, slot {next_pending_slot} is not full");
                break;
            }
            Err(RecvTimeoutError::Disconnected) => {
                println!("all shreds inserted, slot {next_pending_slot} is not full");
                break;
            }
        };
        batches.extend(receiver.try_iter());
        let received_at = Instant::now();
        let measured = start.elapsed() >= measure_from;
        let shreds: Vec<_> = batches
            .into_iter()
            .flat_map(|(sent_at, shreds)| {
                if measured {
                    queue_delays.push(received_at - sent_at);
                }
                shreds
            })
            .collect();
        num_received += shreds.len();
        let num_shreds = shreds.len();

        let insert_start = Instant::now();
        let is_repaired = false;
        let is_trusted = false;
        blockstore
            .insert_shreds_handle_duplicate(
                shreds
                    .into_iter()
                    .map(|shred| (Cow::Owned(shred), is_repaired)),
                is_trusted,
                &mut shred_recovery_context,
                &|_| {},
                &mut metrics,
            )
            .expect("insert should succeed");
        let insert_time = insert_start.elapsed();
        insert_starts.push(insert_start);
        if measured {
            busy += insert_time;
            batch_sizes.push(num_shreds);
            insert_times.push(insert_time);
        }

        while next_pending_slot <= config.num_slots && blockstore.is_full(next_pending_slot) {
            slot_full_at[next_pending_slot as usize - 1] = Some(start.elapsed());
            next_pending_slot += 1;
        }
    }
    let wall = start.elapsed().saturating_sub(measure_from);
    drop(receiver);
    feeder.join().expect("feeder should not panic");
    drop(shred_recovery_context);
    let retransmitted = retransmitter
        .join()
        .expect("retransmit receiver should not panic");

    let completion_latencies: Vec<_> = slot_full_at
        .iter()
        .zip(&schedule.slot_last_broadcast)
        .skip(config.warmup_slots as usize)
        .map(|(full_at, last_broadcast)| {
            full_at.map(|full_at| full_at.saturating_sub(*last_broadcast))
        })
        .collect();
    println!();
    println!(
        "received {num_received} of {total_shreds} shreds, {} of {} slots full",
        slot_full_at.iter().flatten().count(),
        config.num_slots,
    );
    // Includes the turbine jitter of the last FEC set, so a slot keeping up
    // completes within about `BENCH_JITTER_MS` of its end.
    println!("time from the last FEC set broadcast to the slot being full:");
    for (slot, latency) in (config.warmup_slots + 1..).zip(&completion_latencies) {
        match latency {
            Some(latency) => println!("  slot {slot:>3}: {:>8.1} ms", millis(*latency)),
            None => println!("  slot {slot:>3}: not full"),
        }
    }
    let utilization = busy.as_secs_f64() / wall.as_secs_f64();
    let measured_slots = config.num_slots - config.warmup_slots;
    let busy_per_slot = busy / measured_slots as u32;
    // Insert work per slot over the time a slot takes. The insert thread takes
    // whatever is pending, so near capacity this reads close to 100% even when
    // it keeps up: smaller batches cost more per shred.
    let load = busy_per_slot.as_secs_f64() / config.slot_duration.as_secs_f64();
    println!(
        "insert thread busy {:.1}% of the time, {:.1} ms per slot, {:.0}% of the slot duration",
        utilization * 100.0,
        millis(busy_per_slot),
        load * 100.0,
    );
    insert_times.sort_unstable();
    println!(
        "insert call: {} calls, {:.0} shreds per call on average, p50 {:.2} ms, p99 {:.2} ms, max \
         {:.2} ms",
        insert_times.len(),
        batch_sizes.iter().sum::<usize>() as f64 / batch_sizes.len().max(1) as f64,
        millis(percentile(&insert_times, 50)),
        millis(percentile(&insert_times, 99)),
        millis(percentile(&insert_times, 100)),
    );
    queue_delays.sort_unstable();
    println!(
        "queueing delay before insert: p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        millis(percentile(&queue_delays, 50)),
        millis(percentile(&queue_delays, 99)),
        millis(percentile(&queue_delays, 100)),
    );
    println!(
        "blockstore metrics (us, all slots): total {} insert_lock {} insert_shreds {} \
         shred_recovery {} chaining {} commit_working_sets {} write_batch {} index_meta {}",
        metrics.total_elapsed_us,
        metrics.insert_lock_elapsed_us,
        metrics.insert_shreds_elapsed_us,
        metrics.shred_recovery_elapsed_us,
        metrics.chaining_elapsed_us,
        metrics.commit_working_sets_elapsed_us,
        metrics.write_batch_elapsed_us,
        metrics.index_meta_time_us,
    );
    println!(
        "recovered {} data and {} coding shreds, {} inserted",
        metrics.num_recovered_data_shreds,
        metrics.num_recovered_coding_shreds,
        metrics.num_recovered_inserted,
    );
    // Each retransmit batch belongs to the last insert call started before it
    // was received, since inserts run one at a time.
    let measure_from = start + measure_from;
    let mut retransmit_delays = Vec::new();
    let mut num_retransmitted = 0;
    for (received_at, num_shreds) in retransmitted {
        let call = insert_starts.partition_point(|insert_start| *insert_start <= received_at);
        let insert_start = insert_starts[call.checked_sub(1).expect(
            "retransmit batch should be received after the insert call which sent it started",
        )];
        if insert_start >= measure_from {
            retransmit_delays.push(received_at - insert_start);
            num_retransmitted += num_shreds;
        }
    }
    retransmit_delays.sort_unstable();
    println!(
        "recovered shreds retransmit, from insert call start: {} batches, {num_retransmitted} \
         shreds, p50 {:.3} ms, p90 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
        retransmit_delays.len(),
        millis(percentile(&retransmit_delays, 50)),
        millis(percentile(&retransmit_delays, 90)),
        millis(percentile(&retransmit_delays, 99)),
        millis(percentile(&retransmit_delays, 100)),
    );

    let full: Vec<_> = completion_latencies.iter().flatten().collect();
    let half = full.len() / 2;
    let first_half = full[..half]
        .iter()
        .map(|latency| millis(**latency))
        .sum::<f64>()
        / half.max(1) as f64;
    let second_half = full[half..]
        .iter()
        .map(|latency| millis(**latency))
        .sum::<f64>()
        / (full.len() - half).max(1) as f64;
    // Falling behind shows as a backlog: shreds wait for the insert thread
    // longer than a slot, and slots complete later and later.
    let keeps_up = full.len() == completion_latencies.len()
        && percentile(&queue_delays, 99) < config.slot_duration
        && second_half <= first_half * 1.5 + millis(config.slot_duration) / 10.0;
    println!(
        "slot completion latency: first half {first_half:.1} ms, second half {second_half:.1} ms"
    );
    println!(
        "verdict: {}",
        if keeps_up { "keeps up" } else { "falls behind" }
    );
}
