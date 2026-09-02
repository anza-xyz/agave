#![allow(clippy::arithmetic_side_effects)]

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

use {
    clap::{App, Arg, ArgMatches, ErrorKind},
    crossbeam_channel::bounded,
    rand::{Rng, SeedableRng},
    rand_chacha::ChaCha8Rng,
    solana_entry::entry::create_ticks,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        genesis_utils::create_genesis_config_with_leader,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            DATA_SHREDS_PER_FEC_BLOCK, MAX_CODE_SHREDS_PER_SLOT, MAX_DATA_SHREDS_PER_SLOT,
            ProcessShredsStats, ReedSolomonCache, Shred, Shredder,
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
        fmt::{self, Display, Formatter},
        num::NonZeroUsize,
        sync::Arc,
        thread,
        time::{Duration, Instant},
    },
};

const SLOT_DURATION: Duration = Duration::from_millis(400);
const SHRED_FETCH_COALESCE: Duration = Duration::from_millis(5);

// Keep the harness queue deliberately small so saturation mode reaches
// backpressure without requiring the much larger production ingress queue.
const CHANNEL_CAPACITY: usize = 1_024;

const DEFAULT_NUM_SLOTS: usize = 50;
// Preserve the old default workload volume: 500 batches * 64 packets.
const DEFAULT_SHREDS_PER_SLOT: usize = 500 * PACKETS_PER_BATCH;
const DEFAULT_INVALID_PACKETS_PER_SLOT: usize = 0;
const DEFAULT_ARRIVAL_SEED: u64 = 1;

const MAX_SHREDS_PER_SLOT: usize = MAX_DATA_SHREDS_PER_SLOT + MAX_CODE_SHREDS_PER_SLOT;
const FIRST_SLOT: u64 = 1;

#[derive(Clone, Copy, Debug)]
enum ReplayMode {
    Paced,
    Saturate,
}

impl Display for ReplayMode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Paced => "paced",
            Self::Saturate => "saturate",
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum ArrivalDistribution {
    EvenlySpaced,
    UniformRandom,
}

impl Display for ArrivalDistribution {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EvenlySpaced => "evenly-spaced",
            Self::UniformRandom => "uniform-random",
        })
    }
}

#[derive(Debug)]
struct Args {
    mode: ReplayMode,
    distribution: ArrivalDistribution,
    slots: usize,
    shreds_per_slot: usize,
    invalid_per_slot: usize,
    arrival_seed: u64,
    threads: Option<NonZeroUsize>,
}

impl Args {
    fn parse() -> Result<Self, clap::Error> {
        let matches = App::new("shred_sigverify")
            .about("Replay a synthetic shred workload through shred sigverify")
            .arg(
                Arg::with_name("mode")
                    .long("mode")
                    .takes_value(true)
                    .possible_values(&["paced", "saturate"])
                    .default_value("paced")
                    .help("Replay scheduled arrivals or saturate the input channel"),
            )
            .arg(
                Arg::with_name("distribution")
                    .long("distribution")
                    .takes_value(true)
                    .possible_values(&["evenly-spaced", "uniform-random"])
                    .default_value("evenly-spaced")
                    .help("Distribution of shred arrival times within each slot"),
            )
            .arg(
                Arg::with_name("slots")
                    .long("slots")
                    .takes_value(true)
                    .value_name("COUNT")
                    .help("Number of source slots to replay (default: 50)"),
            )
            .arg(
                Arg::with_name("shreds-per-slot")
                    .long("shreds-per-slot")
                    .takes_value(true)
                    .value_name("COUNT")
                    .help("Shreds arriving per source slot (default: 500 packet batches)"),
            )
            .arg(
                Arg::with_name("invalid-per-slot")
                    .long("invalid-per-slot")
                    .takes_value(true)
                    .value_name("COUNT")
                    .help("Intentionally corrupted shred signatures per slot (default: 0)"),
            )
            .arg(
                Arg::with_name("arrival-seed")
                    .long("arrival-seed")
                    .takes_value(true)
                    .value_name("SEED")
                    .help("Seed for random arrival times (default: 1)"),
            )
            .arg(
                Arg::with_name("threads")
                    .long("threads")
                    .takes_value(true)
                    .value_name("COUNT")
                    .help("Number of sigverify workers (default: system thread count)"),
            )
            .get_matches_safe()?;

        Ok(Self {
            mode: match matches.value_of("mode") {
                Some("paced") => ReplayMode::Paced,
                Some("saturate") => ReplayMode::Saturate,
                _ => unreachable!("clap validates --mode"),
            },
            distribution: match matches.value_of("distribution") {
                Some("evenly-spaced") => ArrivalDistribution::EvenlySpaced,
                Some("uniform-random") => ArrivalDistribution::UniformRandom,
                _ => unreachable!("clap validates --distribution"),
            },
            slots: parse_optional(&matches, "slots")?.unwrap_or(DEFAULT_NUM_SLOTS),
            shreds_per_slot: parse_optional(&matches, "shreds-per-slot")?
                .unwrap_or(DEFAULT_SHREDS_PER_SLOT),
            invalid_per_slot: parse_optional(&matches, "invalid-per-slot")?
                .unwrap_or(DEFAULT_INVALID_PACKETS_PER_SLOT),
            arrival_seed: parse_optional(&matches, "arrival-seed")?.unwrap_or(DEFAULT_ARRIVAL_SEED),
            threads: parse_optional(&matches, "threads")?,
        })
    }
}

fn parse_optional<T>(matches: &ArgMatches<'_>, name: &str) -> Result<Option<T>, clap::Error>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    matches
        .value_of(name)
        .map(|value| {
            value
                .parse()
                .map_err(|error| argument_error(format!("invalid value for --{name}: {error}")))
        })
        .transpose()
}

struct ScheduledBatch {
    batch: PacketBatch,
    emit_offset: Duration,
}

struct HarnessConfig {
    replay_mode: ReplayMode,
    arrival_distribution: ArrivalDistribution,
    num_slots: usize,
    shreds_per_slot: usize,
    invalid_packets_per_slot: usize,
    arrival_seed: u64,
    num_sigverify_threads: NonZeroUsize,
    expected_shreds: usize,
    expected_invalid_shreds: usize,
    expected_valid_shreds: usize,
}

impl HarnessConfig {
    fn try_from_args(args: Args) -> Result<Self, clap::Error> {
        if args.slots == 0 {
            return Err(argument_error("--slots must be greater than zero"));
        }
        if args.shreds_per_slot == 0 {
            return Err(argument_error(
                "--shreds-per-slot must be greater than zero",
            ));
        }
        if args.shreds_per_slot > MAX_SHREDS_PER_SLOT {
            return Err(argument_error(format!(
                "--shreds-per-slot={} exceeds the current per-source-slot limit of \
                 {MAX_SHREDS_PER_SLOT} ({MAX_DATA_SHREDS_PER_SLOT} data + \
                 {MAX_CODE_SHREDS_PER_SLOT} coding shreds)",
                args.shreds_per_slot,
            )));
        }
        if args.invalid_per_slot > args.shreds_per_slot {
            return Err(argument_error(
                "--invalid-per-slot must not exceed --shreds-per-slot",
            ));
        }

        let expected_shreds = args
            .slots
            .checked_mul(args.shreds_per_slot)
            .ok_or_else(|| argument_error("configured workload is too large"))?;
        let expected_invalid_shreds = args
            .slots
            .checked_mul(args.invalid_per_slot)
            .ok_or_else(|| argument_error("configured invalid-shred count is too large"))?;
        let expected_valid_shreds = expected_shreds
            .checked_sub(expected_invalid_shreds)
            .ok_or_else(|| argument_error("invalid-shred count exceeds total workload"))?;
        let num_sigverify_threads = args.threads.unwrap_or_else(|| {
            NonZeroUsize::new(get_thread_count())
                .expect("system thread count must be greater than zero")
        });

        Ok(Self {
            replay_mode: args.mode,
            arrival_distribution: args.distribution,
            num_slots: args.slots,
            shreds_per_slot: args.shreds_per_slot,
            invalid_packets_per_slot: args.invalid_per_slot,
            arrival_seed: args.arrival_seed,
            num_sigverify_threads,
            expected_shreds,
            expected_invalid_shreds,
            expected_valid_shreds,
        })
    }
}

impl Display for HarnessConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "Shred sigverify configuration")?;
        writeln!(formatter, "  mode:                 {}", self.replay_mode)?;
        writeln!(
            formatter,
            "  distribution:         {}",
            self.arrival_distribution
        )?;
        writeln!(formatter, "  slots:                {}", self.num_slots)?;
        writeln!(
            formatter,
            "  shreds per slot:      {}",
            self.shreds_per_slot
        )?;
        writeln!(
            formatter,
            "  invalid per slot:     {}",
            self.invalid_packets_per_slot
        )?;
        writeln!(
            formatter,
            "  sigverify threads:    {}",
            self.num_sigverify_threads
        )?;
        writeln!(
            formatter,
            "  total shreds:         {}",
            self.expected_shreds
        )?;
        writeln!(
            formatter,
            "  {:<22}{}",
            "intentionally invalid:", self.expected_invalid_shreds
        )?;
        writeln!(
            formatter,
            "  expected valid:       {}",
            self.expected_valid_shreds
        )?;
        writeln!(formatter, "  maximum per slot:     {MAX_SHREDS_PER_SLOT}")?;
        write!(formatter, "  arrival seed:         {}", self.arrival_seed)
    }
}

fn argument_error(message: impl Into<String>) -> clap::Error {
    clap::Error::with_description(&message.into(), ErrorKind::ValueValidation)
}

struct ReplayResult {
    sent_shreds: usize,
    verified_shreds: usize,
    additional_discards: usize,
    max_observed_input_queue_depth: usize,
    ingress_elapsed: Duration,
    end_to_end_elapsed: Duration,
}

struct BatchShapeStats {
    total_batches: usize,
    full_batches: usize,
    partial_batches: usize,
    min_partial_batch: usize,
    max_partial_batch: usize,
    partial_packets: usize,
}

impl BatchShapeStats {
    fn from_workload(workload: &[ScheduledBatch]) -> Self {
        let mut stats = Self {
            total_batches: workload.len(),
            full_batches: 0,
            partial_batches: 0,
            min_partial_batch: usize::MAX,
            max_partial_batch: 0,
            partial_packets: 0,
        };

        for scheduled_batch in workload {
            let batch_len = scheduled_batch.batch.len();
            assert!(batch_len > 0, "synthetic coalescer produced an empty batch");
            assert!(
                batch_len <= PACKETS_PER_BATCH,
                "synthetic coalescer produced an oversized batch: {batch_len}"
            );

            if batch_len == PACKETS_PER_BATCH {
                stats.full_batches += 1;
            } else {
                stats.partial_batches += 1;
                stats.min_partial_batch = stats.min_partial_batch.min(batch_len);
                stats.max_partial_batch = stats.max_partial_batch.max(batch_len);
                stats.partial_packets += batch_len;
            }
        }

        if stats.partial_batches == 0 {
            stats.min_partial_batch = 0;
        }

        stats
    }

    fn full_batch_ratio(&self) -> f64 {
        if self.total_batches == 0 {
            0.0
        } else {
            self.full_batches as f64 / self.total_batches as f64
        }
    }

    fn average_partial_batch(&self) -> f64 {
        if self.partial_batches == 0 {
            0.0
        } else {
            self.partial_packets as f64 / self.partial_batches as f64
        }
    }
}

struct WorkloadReport<'a> {
    stats: &'a BatchShapeStats,
    scheduled_replay: Duration,
}

impl Display for WorkloadReport<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "Generated packet batches")?;
        writeln!(
            formatter,
            "  total:                {}",
            self.stats.total_batches
        )?;
        writeln!(
            formatter,
            "  full:                 {} ({:.1}%)",
            self.stats.full_batches,
            self.stats.full_batch_ratio() * 100.0
        )?;
        writeln!(
            formatter,
            "  partial:              {}",
            self.stats.partial_batches
        )?;
        writeln!(
            formatter,
            "  partial size:         {} / {:.1} / {} (min / avg / max)",
            self.stats.min_partial_batch,
            self.stats.average_partial_batch(),
            self.stats.max_partial_batch
        )?;
        writeln!(
            formatter,
            "  coalesce window:      {SHRED_FETCH_COALESCE:?}"
        )?;
        write!(
            formatter,
            "  scheduled replay:     {:?}",
            self.scheduled_replay
        )
    }
}

struct ReplayReport<'a> {
    config: &'a HarnessConfig,
    result: &'a ReplayResult,
    nominal_replay: Duration,
    scheduled_replay: Duration,
}

impl Display for ReplayReport<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let shreds_per_second =
            self.result.sent_shreds as f64 / self.result.end_to_end_elapsed.as_secs_f64();
        writeln!(formatter, "Shred sigverify result")?;
        writeln!(
            formatter,
            "  mode:                 {}",
            self.config.replay_mode
        )?;
        writeln!(
            formatter,
            "  sent:                 {}",
            self.result.sent_shreds
        )?;
        writeln!(
            formatter,
            "  {:<22}{}",
            "intentionally invalid:", self.config.expected_invalid_shreds
        )?;
        writeln!(
            formatter,
            "  expected valid:       {}",
            self.config.expected_valid_shreds
        )?;
        writeln!(
            formatter,
            "  verified:             {}",
            self.result.verified_shreds
        )?;
        writeln!(
            formatter,
            "  additional discards:  {}",
            self.result.additional_discards
        )?;
        if matches!(self.config.replay_mode, ReplayMode::Paced) {
            writeln!(
                formatter,
                "  max input queue depth: {}",
                self.result.max_observed_input_queue_depth
            )?;
        }
        writeln!(
            formatter,
            "  ingress elapsed:      {:?}",
            self.result.ingress_elapsed
        )?;
        writeln!(formatter, "  shreds/s:             {shreds_per_second:.0}")?;
        write!(
            formatter,
            "  end-to-end elapsed:   {:?}",
            self.result.end_to_end_elapsed
        )?;
        if matches!(self.config.replay_mode, ReplayMode::Paced) {
            writeln!(formatter)?;
            writeln!(
                formatter,
                "  nominal replay:       {:?}",
                self.nominal_replay
            )?;
            write!(
                formatter,
                "  scheduled replay:     {:?}",
                self.scheduled_replay
            )?;
        }
        Ok(())
    }
}

fn duration_fraction(duration: Duration, numerator: usize, denominator: usize) -> Duration {
    assert!(denominator > 0);

    let nanos = duration
        .as_nanos()
        .checked_mul(numerator as u128)
        .expect("duration multiplication overflowed")
        / denominator as u128;
    Duration::from_nanos(u64::try_from(nanos).expect("duration does not fit in u64 nanoseconds"))
}

fn duration_mul(duration: Duration, factor: usize) -> Duration {
    let nanos = duration
        .as_nanos()
        .checked_mul(factor as u128)
        .expect("duration multiplication overflowed");
    Duration::from_nanos(u64::try_from(nanos).expect("duration does not fit in u64 nanoseconds"))
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

fn make_slot_shreds(leader_keypair: &Keypair, slot: u64, shreds_per_slot: usize) -> Vec<Shred> {
    // The current shred format uses 32 data + 32 coding shreds per FEC block.
    // Split the synthetic source-slot workload between both kinds so the
    // configured total cannot silently exceed either per-slot index limit.
    let target_data_shreds = shreds_per_slot.div_ceil(2);
    let target_coding_shreds = shreds_per_slot / 2;

    assert!(target_data_shreds <= MAX_DATA_SHREDS_PER_SLOT);
    assert!(target_coding_shreds <= MAX_CODE_SHREDS_PER_SLOT);

    let shred_size = shred_size_typical();
    let target_data_shreds_u64 =
        u64::try_from(target_data_shreds).expect("data shred count does not fit in u64");
    let num_ticks = max_ticks_per_n_shreds(target_data_shreds_u64, Some(shred_size));

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

    assert!(
        data_shreds.len() >= target_data_shreds,
        "shred generator produced {} data shreds, expected at least {}",
        data_shreds.len(),
        target_data_shreds,
    );
    assert!(
        coding_shreds.len() >= target_coding_shreds,
        "shred generator produced {} coding shreds, expected at least {}",
        coding_shreds.len(),
        target_coding_shreds,
    );

    // Interleave data and coding shreds instead of putting all data traffic
    // first and all coding traffic second.
    let mut data_shreds = data_shreds.into_iter().take(target_data_shreds);
    let mut coding_shreds = coding_shreds.into_iter().take(target_coding_shreds);
    let mut shreds = Vec::with_capacity(shreds_per_slot);

    loop {
        let mut pushed = false;

        if let Some(shred) = data_shreds.next() {
            shreds.push(shred);
            pushed = true;
        }
        if let Some(shred) = coding_shreds.next() {
            shreds.push(shred);
            pushed = true;
        }

        if !pushed {
            break;
        }
    }

    assert_eq!(shreds.len(), shreds_per_slot);
    shreds
}

fn evenly_spaced_arrival_offsets(shreds_per_slot: usize) -> Vec<Duration> {
    (0..shreds_per_slot)
        .map(|packet_index| duration_fraction(SLOT_DURATION, packet_index, shreds_per_slot))
        .collect()
}

fn uniform_random_arrival_offsets(shreds_per_slot: usize, rng: &mut ChaCha8Rng) -> Vec<Duration> {
    let slot_nanos =
        u64::try_from(SLOT_DURATION.as_nanos()).expect("slot duration must fit in u64 nanoseconds");

    // Sample packet arrival times independently across the slot, then sort
    // them into receive order. The arrival model intentionally knows nothing
    // about the coalescer; batch boundaries are derived later.
    let mut offsets = (0..shreds_per_slot)
        .map(|_| Duration::from_nanos(rng.random_range(0..slot_nanos)))
        .collect::<Vec<_>>();

    offsets.sort_unstable();
    offsets
}

fn slot_arrival_offsets(
    distribution: ArrivalDistribution,
    shreds_per_slot: usize,
    rng: &mut ChaCha8Rng,
) -> Vec<Duration> {
    match distribution {
        ArrivalDistribution::EvenlySpaced => evenly_spaced_arrival_offsets(shreds_per_slot),
        ArrivalDistribution::UniformRandom => uniform_random_arrival_offsets(shreds_per_slot, rng),
    }
}

/// Stateful approximation of the shred-fetch UDP receive loop.
///
/// Production starts the coalescing deadline when it enters a receive cycle,
/// not when the first packet arrives. Consequently, if the receiver blocks
/// waiting for data past that deadline, its first successful read completes
/// the batch immediately. This models that distinction while intentionally
/// ignoring recv_mmsg/poll syscall details.
struct BatchCoalescer {
    workload: Vec<ScheduledBatch>,
    batch: RecycledPacketBatch,
    receive_deadline: Duration,
    last_arrival_offset: Option<Duration>,
}

impl BatchCoalescer {
    fn new(expected_packets: usize) -> Self {
        Self {
            workload: Vec::with_capacity(expected_packets.div_ceil(PACKETS_PER_BATCH)),
            batch: RecycledPacketBatch::with_capacity(PACKETS_PER_BATCH),
            receive_deadline: SHRED_FETCH_COALESCE,
            last_arrival_offset: None,
        }
    }

    fn push(&mut self, packet: Packet, arrival_offset: Duration) {
        if let Some(previous_arrival) = self.last_arrival_offset {
            assert!(
                arrival_offset >= previous_arrival,
                "packet arrivals must be ordered"
            );
        }
        self.last_arrival_offset = Some(arrival_offset);

        if !self.batch.is_empty() && arrival_offset >= self.receive_deadline {
            self.flush(self.receive_deadline);
            self.receive_deadline = self
                .receive_deadline
                .checked_add(SHRED_FETCH_COALESCE)
                .expect("receive deadline overflowed");
        }

        // recv_from_coalesce receives at least one packet before checking an
        // already-expired cycle deadline. Model that packet as being emitted
        // at its actual arrival time.
        if self.batch.is_empty() && arrival_offset >= self.receive_deadline {
            self.batch.push(packet);
            self.flush(arrival_offset);
            self.receive_deadline = arrival_offset
                .checked_add(SHRED_FETCH_COALESCE)
                .expect("receive deadline overflowed");
            return;
        }

        self.batch.push(packet);

        if self.batch.len() == PACKETS_PER_BATCH {
            self.flush(arrival_offset);
            self.receive_deadline = arrival_offset
                .checked_add(SHRED_FETCH_COALESCE)
                .expect("receive deadline overflowed");
        }
    }

    fn flush(&mut self, emit_offset: Duration) {
        assert!(!self.batch.is_empty(), "cannot emit an empty packet batch");

        let batch = std::mem::replace(
            &mut self.batch,
            RecycledPacketBatch::with_capacity(PACKETS_PER_BATCH),
        );
        self.workload.push(ScheduledBatch {
            batch: PacketBatch::from(batch),
            emit_offset,
        });
    }

    fn finish(mut self) -> Vec<ScheduledBatch> {
        if !self.batch.is_empty() {
            self.flush(self.receive_deadline);
        }
        self.workload
    }
}

fn make_workload(leader_keypair: &Keypair, config: &HarnessConfig) -> Vec<ScheduledBatch> {
    let mut rng = ChaCha8Rng::seed_from_u64(config.arrival_seed);
    let mut coalescer = BatchCoalescer::new(config.expected_shreds);

    for slot_offset in 0..config.num_slots {
        let slot = FIRST_SLOT
            .checked_add(u64::try_from(slot_offset).expect("slot offset does not fit in u64"))
            .expect("slot number overflowed");
        let slot_start = duration_mul(SLOT_DURATION, slot_offset);

        let shreds = make_slot_shreds(leader_keypair, slot, config.shreds_per_slot);
        let arrivals = slot_arrival_offsets(
            config.arrival_distribution,
            config.shreds_per_slot,
            &mut rng,
        );

        assert_eq!(shreds.len(), arrivals.len());

        let mut corrupted_packets = 0usize;

        for (packet_index, (shred, slot_arrival)) in shreds.into_iter().zip(arrivals).enumerate() {
            let mut packet = shred.payload().to_packet(None);

            if should_corrupt_packet(
                packet_index,
                config.shreds_per_slot,
                config.invalid_packets_per_slot,
            ) {
                corrupt_shred_signature(&mut packet);
                corrupted_packets += 1;
            }

            coalescer.push(packet, slot_start + slot_arrival);
        }

        assert_eq!(corrupted_packets, config.invalid_packets_per_slot);
    }

    coalescer.finish()
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

type VerifiedShred = (
    solana_ledger::shred::Payload,
    bool,
    solana_ledger::blockstore_meta::BlockLocation,
);

fn run_sigverify(
    config: &HarnessConfig,
    leader_keypair: &Keypair,
    workload: Vec<ScheduledBatch>,
) -> ReplayResult {
    let leader_pubkey = leader_keypair.pubkey();

    // The validator running sigverify must not be the leader.
    let node_keypair = Arc::new(Keypair::new());
    let node_pubkey = node_keypair.pubkey();

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

    // Retransmit output is intentionally ignored by this harness. EvictingSender
    // retains the receiver it needs to evict old messages from a full channel.
    let (retransmit_sender, _) = EvictingSender::new_bounded(1);

    // Drain verified output continuously so sigverify cannot block on the
    // downstream channel.
    let (verified_sender, verified_receiver) = bounded::<Vec<VerifiedShred>>(CHANNEL_CAPACITY);
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
        config.num_sigverify_threads,
    );

    let replay_start = Instant::now();
    let mut sent_shreds = 0usize;
    let mut max_observed_input_queue_depth = 0usize;

    for ScheduledBatch { batch, emit_offset } in workload {
        let batch_len = batch.len();

        match config.replay_mode {
            ReplayMode::Paced => {
                sleep_until(replay_start + emit_offset);
                shred_fetch_sender
                    .try_send(batch)
                    .expect("shred sigverify input channel must not be full");
                max_observed_input_queue_depth =
                    max_observed_input_queue_depth.max(shred_fetch_sender.len());
            }
            ReplayMode::Saturate => {
                shred_fetch_sender
                    .send(batch)
                    .expect("shred sigverify receiver disconnected");
            }
        }

        sent_shreds = sent_shreds
            .checked_add(batch_len)
            .expect("sent shred count overflowed");
    }

    let ingress_elapsed = replay_start.elapsed();
    assert_eq!(sent_shreds, config.expected_shreds);

    // Closing ingress lets sigverify drain all queued batches and terminate.
    drop(shred_fetch_sender);
    sigverify_handle
        .join()
        .expect("shred sigverify thread panicked");
    let verified_shreds = verified_handle
        .join()
        .expect("verified shred consumer panicked");
    let end_to_end_elapsed = replay_start.elapsed();

    assert!(
        verified_shreds <= config.expected_valid_shreds,
        "sigverify emitted {verified_shreds} shreds, but only {} input shreds had valid leader \
         signatures",
        config.expected_valid_shreds,
    );

    let additional_discards = config
        .expected_valid_shreds
        .checked_sub(verified_shreds)
        .expect("verified shred count exceeds expected-valid count");

    // Deduper false positives are expected, but dropping more than 1% of
    // otherwise valid shreds indicates a broken benchmark or sigverify regression.
    let max_additional_discards = config.expected_valid_shreds.div_ceil(100);

    assert!(
        additional_discards <= max_additional_discards,
        "sigverify discarded {additional_discards} valid shreds; maximum allowed is \
         {max_additional_discards} (1%)"
    );

    ReplayResult {
        sent_shreds,
        verified_shreds,
        additional_discards,
        max_observed_input_queue_depth,
        ingress_elapsed,
        end_to_end_elapsed,
    }
}

fn main() {
    let args = Args::parse().unwrap_or_else(|error| error.exit());
    let config = HarnessConfig::try_from_args(args).unwrap_or_else(|error| error.exit());

    println!("{config}\n");

    let leader_keypair = Keypair::new();

    // Generate and coalesce all input before entering the measured section.
    let workload = make_workload(&leader_keypair, &config);
    let batch_stats = BatchShapeStats::from_workload(&workload);
    let scheduled_replay = workload
        .last()
        .map(|batch| batch.emit_offset)
        .unwrap_or_default();
    let generated_shreds = workload
        .iter()
        .map(|scheduled_batch| scheduled_batch.batch.len())
        .sum::<usize>();

    assert_eq!(generated_shreds, config.expected_shreds);

    println!(
        "{}\n",
        WorkloadReport {
            stats: &batch_stats,
            scheduled_replay,
        }
    );

    println!(
        "Shred sigverify workload ready; attach profiler to pid {}\n",
        std::process::id()
    );

    let result = run_sigverify(&config, &leader_keypair, workload);
    let nominal_replay = duration_mul(SLOT_DURATION, config.num_slots);

    println!(
        "{}",
        ReplayReport {
            config: &config,
            result: &result,
            nominal_replay,
            scheduled_replay,
        }
    );
}
