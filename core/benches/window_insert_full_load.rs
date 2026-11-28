//! Sustained load generator for the window service.
//!
//! Every slot a predetermined number of FEC sets. Shreds arrive over the
//! slot as a leader would broadcast them, with turbine jitter and loss emulated,
//! and are handed as `VerifiedShred` batches, same as from shred sigverify, to the
//! `solWinInsert` thread of window service, which inserts them into the blockstore.
//! The report shows whether the insert thread keeps up with the slot rate or falls behind,
//! and how long it takes recovered shreds to reach retransmit.
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
//! `cargo bench -p solana-core --bench window_insert_full_load`
#![allow(clippy::arithmetic_side_effects)]
use {
    agave_worker_pool::{WorkerPool, WorkerPoolConfig},
    crossbeam_channel::{Sender, unbounded},
    rand::Rng,
    solana_clock::Slot,
    solana_core::window_service::WindowService,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::Blockstore,
        blockstore_meta::BlockLocation,
        genesis_utils::create_genesis_config,
        get_tmp_ledger_path_auto_delete,
        shred::{
            DATA_SHREDS_PER_FEC_BLOCK, MAX_DATA_SHREDS_PER_SLOT, ProcessShredsStats, Shred,
            Shredder, layout,
        },
        sigverify_shreds::VerifiedShred,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    std::{
        collections::HashMap,
        env, fs,
        num::NonZeroUsize,
        str::FromStr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
    tempfile::TempDir,
};

const SHRED_VERSION: u16 = 1;
const TXS_PER_ENTRY: usize = 64;
// Data shred limit of 200ms slots, see `SLOT_PARAMS_200MS` in runtime.
const FEC_SETS_PER_200MS_SLOT: usize = 16_384 / DATA_SHREDS_PER_FEC_BLOCK;
const INSERT_THREAD_NAME: &str = "solWinInsert";
// Give up when no slot completes for this long.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(1);

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

/// Identifies an FEC set by slot and FEC set index.
type FecSetId = (Slot, u32);

struct Arrival {
    offset: Duration,
    fec_set: FecSetId,
    /// Verified as shred sigverify hands it to window service, with the
    /// Merkle root already computed.
    shred: VerifiedShred,
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
            let first = data_shreds
                .first()
                .expect("FEC set should have data shreds");
            let fec_set_id = (slot, first.fec_set_index());
            let merkle_root = first
                .merkle_root()
                .expect("merkle shreds should have a merkle root");
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
                    fec_set: fec_set_id,
                    shred: VerifiedShred {
                        shred: shred.clone(),
                        is_repaired: false,
                        location: BlockLocation::Original,
                        merkle_root,
                    },
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
/// like those shred sigverify sends to window service. Returns when each
/// shred was sent, along with its FEC set, and the most batches the insert
/// thread had pending.
fn run_feeder(
    arrivals: Vec<Arrival>,
    batch_interval: Duration,
    start: Instant,
    sender: Sender<Vec<VerifiedShred>>,
) -> (Vec<(FecSetId, Instant)>, usize) {
    let mut sent = Vec::with_capacity(arrivals.len());
    let mut max_queue_len = 0;
    let mut arrivals = arrivals.into_iter().peekable();
    while let Some(next) = arrivals.peek() {
        let now = start.elapsed();
        if next.offset > now {
            thread::sleep((next.offset - now).max(batch_interval));
            continue;
        }
        let now = start.elapsed();
        let mut batch = Vec::new();
        let mut fec_sets = Vec::new();
        while let Some(arrival) = arrivals.next_if(|arrival| arrival.offset <= now) {
            fec_sets.push(arrival.fec_set);
            batch.push(arrival.shred);
        }
        let sent_at = Instant::now();
        if sender.send(batch).is_err() {
            break;
        }
        max_queue_len = max_queue_len.max(sender.len());
        sent.extend(fec_sets.into_iter().map(|fec_set| (fec_set, sent_at)));
        thread::sleep(batch_interval);
    }
    (sent, max_queue_len)
}

/// Returns the CPU time the thread named `name` of this process has used.
fn thread_cpu_time(name: &str) -> Duration {
    let tasks = fs::read_dir("/proc/self/task").expect("/proc/self/task should be readable");
    for task in tasks {
        let path = task.expect("task entry should be readable").path();
        let Ok(comm) = fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if comm.trim_end() != name {
            continue;
        }
        let schedstat = fs::read_to_string(path.join("schedstat"))
            .expect("schedstat of a live thread should be readable");
        let nanos = schedstat
            .split_whitespace()
            .next()
            .and_then(|field| field.parse().ok())
            .expect("schedstat should start with the CPU time in ns");
        return Duration::from_nanos(nanos);
    }
    panic!("thread {name} should be running");
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
    let blockstore = Arc::new(Blockstore::open(ledger_dir.path()).expect("blockstore should open"));
    let genesis_config = create_genesis_config(1).genesis_config;
    let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
    let sharable_banks = bank_forks
        .read()
        .expect("bank forks lock should not be poisoned")
        .sharable_banks();

    // Recovered shreds are sent for retransmit from inside the insert thread.
    // Timestamp them on arrival at the retransmit pool.
    let (retransmitted_sender, retransmitted_receiver) = unbounded();
    let retransmit_sender = WorkerPool::spawn(
        WorkerPoolConfig {
            job_queue_capacity: NonZeroUsize::new(1024).expect("capacity should be non-zero"),
            ..WorkerPoolConfig::new("solBenchRetx", NonZeroUsize::MIN)
        },
        move |_| {
            let sender = retransmitted_sender.clone();
            move |shreds| {
                sender
                    .send((Instant::now(), shreds))
                    .expect("retransmit receiver should outlive the retransmit pool");
            }
        },
    )
    .expect("retransmit pool should spawn");

    let exit = Arc::new(AtomicBool::new(false));
    let (verified_sender, verified_receiver) = unbounded();
    // Kept alive so that the insert thread can send possible duplicates.
    let (duplicate_sender, _duplicate_receiver) = unbounded();
    let completed_data_sets_sender = None;
    let insert_thread = WindowService::start_window_insert_thread(
        exit.clone(),
        blockstore.clone(),
        sharable_banks,
        SHRED_VERSION,
        verified_receiver,
        duplicate_sender,
        completed_data_sets_sender,
        Arc::new(retransmit_sender),
    );

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
    // Keeps the insert thread alive after the feeder finishes, so its CPU time
    // can still be read once the last slot is full.
    let verified_sender_guard = verified_sender.clone();
    let start = Instant::now();
    let feeder = {
        let batch_interval = config.batch_interval;
        let arrivals = schedule.arrivals;
        thread::spawn(move || run_feeder(arrivals, batch_interval, start, verified_sender))
    };

    let measure_from = config.slot_duration * config.warmup_slots as u32;
    let mut cpu_at_measure_from = None;
    let mut slot_full_at = vec![None; config.num_slots as usize];
    let mut next_pending_slot = 1;
    let mut last_progress = Instant::now();
    while next_pending_slot <= config.num_slots {
        if cpu_at_measure_from.is_none() && start.elapsed() >= measure_from {
            cpu_at_measure_from = Some(thread_cpu_time(INSERT_THREAD_NAME));
        }
        while next_pending_slot <= config.num_slots && blockstore.is_full(next_pending_slot) {
            slot_full_at[next_pending_slot as usize - 1] = Some(start.elapsed());
            next_pending_slot += 1;
            last_progress = Instant::now();
        }
        if last_progress.elapsed() > STALL_TIMEOUT {
            println!(
                "no slot completed for {STALL_TIMEOUT:?}, slot {next_pending_slot} is not full"
            );
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let wall = start.elapsed().saturating_sub(measure_from);
    let cpu = thread_cpu_time(INSERT_THREAD_NAME)
        - cpu_at_measure_from.expect("measurement should start before the last slot completes");
    let (sent, max_queue_len) = feeder.join().expect("feeder should not panic");
    drop(verified_sender_guard);
    exit.store(true, Ordering::Relaxed);
    insert_thread
        .join()
        .expect("insert thread should not panic");
    // The insert thread owned the last reference to the retransmit pool, so
    // joining it drained the pool and closed the channel.
    let retransmitted: Vec<_> = retransmitted_receiver.iter().collect();

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
        "sent {} of {total_shreds} shreds, {} of {} slots full",
        sent.len(),
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
    let measured_slots = config.num_slots - config.warmup_slots;
    let cpu_per_slot = cpu / measured_slots as u32;
    // Insert work per slot over the time a slot takes. The insert thread takes
    // whatever is pending, so near capacity this reads close to 100% even when
    // it keeps up: smaller batches cost more per shred, and FEC sets are
    // recovered sooner, before more of their shreds arrive over turbine.
    let load = cpu_per_slot.as_secs_f64() / config.slot_duration.as_secs_f64();
    println!(
        "{INSERT_THREAD_NAME} CPU {:.1}% of the time, {:.1} ms per slot, {:.2} us per shred, \
         {:.0}% of the slot duration",
        cpu.as_secs_f64() / wall.as_secs_f64() * 100.0,
        millis(cpu_per_slot),
        cpu_per_slot.as_secs_f64() * 1e6 / schedule.num_shreds_per_slot as f64,
        load * 100.0,
    );

    // The insert thread recovers an FEC set once it holds as many shreds as
    // the set has data shreds, so latency counts from when that shred was sent.
    let mut fec_set_sends: HashMap<FecSetId, Vec<Instant>> = HashMap::new();
    for (fec_set, sent_at) in sent {
        fec_set_sends.entry(fec_set).or_default().push(sent_at);
    }
    let recoverable_at: HashMap<_, _> = fec_set_sends
        .into_iter()
        .filter_map(|(fec_set, mut sends)| {
            sends.sort_unstable();
            Some((fec_set, *sends.get(DATA_SHREDS_PER_FEC_BLOCK - 1)?))
        })
        .collect();
    let mut retransmit_delays = Vec::new();
    for (received_at, shreds) in &retransmitted {
        for shred in shreds {
            let shred = shred.as_ref();
            let slot = layout::get_slot(shred).expect("recovered shred should have a slot");
            if slot <= config.warmup_slots {
                continue;
            }
            let fec_set_index = layout::get_fec_set_index(shred)
                .expect("recovered shred should have an FEC set index");
            let recoverable_at = recoverable_at
                .get(&(slot, fec_set_index))
                .expect("recovered FEC set should have been sent");
            retransmit_delays.push(received_at.saturating_duration_since(*recoverable_at));
        }
    }
    retransmit_delays.sort_unstable();
    println!(
        "recovered shreds retransmit, from the FEC set being recoverable: {} shreds, p50 {:.3} \
         ms, p90 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
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
    // Falling behind shows as a backlog: batches wait for the insert thread
    // longer than a slot, and slots complete later and later.
    let max_backlog = config.batch_interval * max_queue_len as u32;
    println!(
        "most batches pending insert: {max_queue_len}, about {:.1} ms of arrivals",
        millis(max_backlog)
    );
    let keeps_up = full.len() == completion_latencies.len()
        && max_backlog < config.slot_duration
        && second_half <= first_half * 1.5 + millis(config.slot_duration) / 10.0;
    println!(
        "slot completion latency: first half {first_half:.1} ms, second half {second_half:.1} ms"
    );
    println!(
        "verdict: {}",
        if keeps_up { "keeps up" } else { "falls behind" }
    );
}
