//! Small example driver for measuring the BLS sigverify vote path.
//!
//! This example first constructs synthetic vote `PacketBatch` inputs and then
//! measures only the time spent feeding those prebuilt batches into the same
//! verification path used by the BLS sigverifier.
//!
//! Input construction is performed before timing starts so that the reported
//! results reflect verifier-side processing rather than packet generation.
//!
//! Arguments:
//! - `--batch-size`: number of packets per generated `PacketBatch`
//! - `--total-packets`: total number of packets processed in the run
//! - `--bad-ratio`: fraction of invalid signatures in `[0.0, 1.0]`
//! - `--two-slot-mode`: alternate votes between two slots instead of using one slot
//! - `--seed`: RNG seed for deterministic bad-signature placement
//! - `--num-threads`: Rayon thread count used by the verifier
//! - `--num-validators`: size of the synthetic validator set
//! - `--csv`: emit CSV output instead of human-readable output
//!
//! Example:
//!   cargo run -p solana-core --features dev-context-only-utils --example sigverify_perf -- \
//!       --batch-size 32 --total-packets 65536 --bad-ratio 0.005 --seed 42 \
//!       --num-threads 4 --num-validators 2000
//!
//! Two-slot example:
//!   cargo run -p solana-core --features dev-context-only-utils --example sigverify_perf -- \
//!       --batch-size 32 --total-packets 65536 --bad-ratio 0.005 --two-slot-mode --seed 42 \
//!       --num-threads 4 --num-validators 2000
//!
//! CSV example:
//!   cargo run -p solana-core --features dev-context-only-utils --example sigverify_perf -- \
//!       --csv --batch-size 32 --total-packets 65536 --bad-ratio 0.0 \
//!       --seed 42 --num-threads 4 --num-validators 2000
//!
//! CSV output format:
//!   seed,batch_size,total_packets,bad_ratio,two_slot_mode,num_threads,num_validators,\
//!   bad_packets,valid_packets,elapsed_us,per_packet_us,per_valid_packet_us
//!
//! Companion scripts for repeated runs, perf collection, and plotting:
//!   https://gist.github.com/igorbologovv/e0afa7ef1312519538998dbd06bad5b7

extern crate clap4 as clap;

use {
    agave_votor::{
        consensus_metrics::ConsensusMetricsEvent, generated_cert_types::GeneratedCertTypes,
    },
    agave_votor_messages::{
        consensus_message::{ConsensusMessage, VoteMessage},
        migration::MigrationStatus,
        reward_certificate::AddVoteMessage,
        vote::Vote,
    },
    clap::Parser,
    crossbeam_channel::{Receiver, bounded},
    rand::{Rng, SeedableRng, rngs::StdRng},
    solana_bls_signatures::Signature,
    solana_core::{
        bls_sigverify::bls_sigverifier::{SigVerifier, SigVerifierChannels, SigVerifierContext},
        cluster_info_vote_listener::VerifiedVoterSlotsReceiver,
    },
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::{Keypair, Signer},
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_net_utils::SocketAddrSpace,
    solana_perf::packet::{Packet, PacketBatch, RecycledPacketBatch},
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
    },
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{convert::TryFrom, sync::Arc, time::Instant},
};

const CHANNEL_SIZE: usize = 1024;

#[derive(Debug, Parser)]
#[command(
    name = "sigverify_perf",
    about = "Measure the BLS sigverify vote path with synthetic vote batches"
)]
struct Config {
    #[arg(long, help = "Emit CSV output instead of human-readable output")]
    csv: bool,

    #[arg(long, help = "RNG seed used for deterministic bad-signature placement")]
    seed: u64,

    #[arg(
        long = "batch-size",
        help = "Number of packets per generated PacketBatch"
    )]
    batch_size: usize,

    #[arg(long = "total-packets", help = "Total number of packets to generate")]
    total_packets: usize,

    #[arg(
        long = "bad-ratio",
        help = "Fraction of invalid signatures in [0.0, 1.0]. Use 0.0 for the clean case"
    )]
    bad_ratio: f64,

    #[arg(
        long = "two-slot-mode",
        help = "Alternate votes between two slots instead of using a single slot"
    )]
    two_slot_mode: bool,

    #[arg(
        long = "num-threads",
        help = "Thread count for the verifier thread pool"
    )]
    num_threads: usize,

    #[arg(long = "num-validators", help = "Size of the synthetic validator set")]
    num_validators: usize,
}

#[derive(Debug, serde::Serialize)]
struct OutputRow {
    seed: u64,
    batch_size: usize,
    total_packets: usize,
    bad_ratio: f64,
    two_slot_mode: bool,
    num_threads: usize,
    num_validators: usize,
    bad_packets: usize,
    valid_packets: usize,
    elapsed_us: u64,
    per_packet_us: u64,
    per_valid_packet_us: u64,
}

struct ExampleContext {
    verifier: SigVerifier,
    validator_keypairs: Vec<ValidatorVoteKeypairs>,
    validator_ranks: Vec<u16>,
    _repair_receiver: VerifiedVoterSlotsReceiver,
    _reward_receiver: Receiver<AddVoteMessage>,
    _pool_receiver: Receiver<Vec<ConsensusMessage>>,
    _metrics_receiver: Receiver<(std::time::Instant, Vec<ConsensusMetricsEvent>)>,
}

struct BadPacketSelector {
    rng: StdRng,
    remaining_packets: usize,
    remaining_bad: usize,
}

impl BadPacketSelector {
    fn new(total_packets: usize, bad_ratio: f64, seed: u64) -> Self {
        let remaining_bad = ((total_packets as f64) * bad_ratio).round() as usize;
        Self {
            rng: StdRng::seed_from_u64(seed),
            remaining_packets: total_packets,
            remaining_bad,
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn next_is_bad(&mut self) -> bool {
        debug_assert!(self.remaining_packets > 0);

        if self.remaining_bad == 0 {
            self.remaining_packets -= 1;
            return false;
        }

        if self.remaining_bad == self.remaining_packets {
            self.remaining_bad -= 1;
            self.remaining_packets -= 1;
            return true;
        }

        let is_bad = self.rng.random_ratio(
            u32::try_from(self.remaining_bad).expect("remaining_bad must fit into u32"),
            u32::try_from(self.remaining_packets).expect("remaining_packets must fit into u32"),
        );

        self.remaining_packets -= 1;
        if is_bad {
            self.remaining_bad -= 1;
        }

        is_bad
    }
}

#[allow(clippy::arithmetic_side_effects)]
fn init_example_context(num_threads: usize, num_validators: usize) -> ExampleContext {
    let validator_keypairs: Vec<_> = (0..num_validators)
        .map(|_| ValidatorVoteKeypairs::new_rand())
        .collect();

    let stakes: Vec<_> = (0..validator_keypairs.len()).map(|_| 1_000_u64).collect();

    let genesis = create_genesis_config_with_alpenglow_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        stakes,
    );

    let bank0 = Bank::new_for_tests(&genesis.genesis_config);
    let (bank0, _temp_bank_forks) = bank0.wrap_with_bank_forks_for_tests();

    let mut parent = bank0;
    let mut bank = Bank::new_from_parent(
        parent.clone(),
        solana_runtime::bank::SlotLeader::default(),
        1,
    );

    for i in 2..10 {
        parent = Arc::new(bank);
        bank = Bank::new_from_parent(
            parent.clone(),
            solana_runtime::bank::SlotLeader::default(),
            i,
        );
    }

    let bank_forks = BankForks::new_rw_arc(bank);
    let sharable_banks = bank_forks
        .read()
        .expect("bank_forks poisoned")
        .sharable_banks();

    let root_bank = sharable_banks.root();
    let rank_map = root_bank
        .get_rank_map(10)
        .expect("rank map for slot 10 must exist in bench");

    let validator_ranks: Vec<u16> = validator_keypairs
        .iter()
        .map(|validator| {
            let validator_bls_pubkey = validator.bls_keypair.public;

            (0..validator_keypairs.len())
                .find_map(|i| {
                    rank_map.get_pubkey_stake_entry(i).and_then(|entry| {
                        (entry.bls_pubkey == validator_bls_pubkey)
                            .then_some(u16::try_from(i).expect("validator index must fit into u16"))
                    })
                })
                .expect("validator BLS pubkey must exist in rank map")
        })
        .collect();

    let keypair = Keypair::new();
    let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);

    let cluster_info = Arc::new(ClusterInfo::new(
        contact_info,
        Arc::new(keypair),
        SocketAddrSpace::Unspecified,
    ));

    let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

    let (repair_sender, repair_receiver) = bounded(CHANNEL_SIZE);
    let (reward_sender, reward_receiver) = bounded(CHANNEL_SIZE);
    let (pool_sender, pool_receiver) = bounded(CHANNEL_SIZE);
    let (metrics_sender, metrics_receiver) = bounded(CHANNEL_SIZE);
    let (_packet_sender, packet_receiver) = bounded(CHANNEL_SIZE);

    let banlist = {
        let (banlist, _) = SimpleQosBanlist::new();
        Arc::new(banlist)
    };

    let generated_cert_types = Arc::new(GeneratedCertTypes::default());

    let verifier = SigVerifier::new(
        SigVerifierContext::new(
            Arc::new(MigrationStatus::default()),
            banlist,
            sharable_banks,
            cluster_info,
            leader_schedule,
            num_threads,
            generated_cert_types,
        ),
        SigVerifierChannels::new(
            packet_receiver,
            repair_sender,
            reward_sender,
            pool_sender,
            metrics_sender,
        ),
    );

    ExampleContext {
        verifier,
        validator_keypairs,
        validator_ranks,
        _repair_receiver: repair_receiver,
        _reward_receiver: reward_receiver,
        _pool_receiver: pool_receiver,
        _metrics_receiver: metrics_receiver,
    }
}

fn validate_config(config: &Config) -> Result<(), String> {
    if config.batch_size == 0 {
        return Err("batch_size must be > 0".to_string());
    }
    if config.total_packets == 0 {
        return Err("total_packets must be > 0".to_string());
    }
    if !(0.0..=1.0).contains(&config.bad_ratio) {
        return Err("bad_ratio must be between 0.0 and 1.0".to_string());
    }
    if config.num_threads == 0 {
        return Err("num_threads must be > 0".to_string());
    }
    if config.num_validators == 0 {
        return Err("num_validators must be > 0".to_string());
    }

    Ok(())
}

#[allow(clippy::arithmetic_side_effects)]
fn make_vote(global_index: usize, two_slot_mode: bool) -> Vote {
    let slot = if two_slot_mode {
        10 + (global_index % 2) as u64
    } else {
        10
    };
    Vote::new_skip_vote(slot)
}

#[allow(clippy::arithmetic_side_effects)]
fn make_vote_batch(
    ctx: &ExampleContext,
    start_index: usize,
    batch_size: usize,
    two_slot_mode: bool,
    bad_selector: &mut BadPacketSelector,
) -> (PacketBatch, usize) {
    let mut packets = Vec::with_capacity(batch_size);
    let mut bad_packets = 0_usize;

    for i in 0..batch_size {
        let global_index = start_index + i;
        let validator_index = global_index % ctx.validator_keypairs.len();

        let validator = &ctx.validator_keypairs[validator_index];
        let rank = ctx.validator_ranks[validator_index];

        let vote = make_vote(global_index, two_slot_mode);
        let payload = wincode::serialize(&vote).expect("failed to serialize vote");

        let mut signature = validator.bls_keypair.sign(&payload).into();

        if bad_selector.next_is_bad() {
            signature = Signature::default();
            bad_packets += 1;
        }

        let vote_msg = VoteMessage {
            vote,
            signature,
            rank,
        };

        let msg = ConsensusMessage::Vote(vote_msg);

        let mut packet = Packet::default();
        packet
            .populate_packet(None, &msg)
            .expect("failed to populate packet");
        packet
            .meta_mut()
            .set_remote_pubkey(validator.node_keypair.pubkey());

        packets.push(packet);
    }

    (RecycledPacketBatch::new(packets).into(), bad_packets)
}

#[allow(clippy::arithmetic_side_effects)]
fn build_all_vote_batches(
    ctx: &ExampleContext,
    total_packets: usize,
    batch_size: usize,
    two_slot_mode: bool,
    bad_ratio: f64,
    seed: u64,
) -> (Vec<PacketBatch>, usize) {
    let mut bad_selector = BadPacketSelector::new(total_packets, bad_ratio, seed);
    let mut batches = Vec::new();
    let mut processed = 0_usize;
    let mut bad_packets_generated = 0_usize;

    while processed < total_packets {
        let remaining = total_packets - processed;
        let actual = remaining.min(batch_size);

        let (batch, bad_in_batch) =
            make_vote_batch(ctx, processed, actual, two_slot_mode, &mut bad_selector);

        batches.push(batch);
        bad_packets_generated += bad_in_batch;
        processed += actual;
    }

    (batches, bad_packets_generated)
}

fn print_results(row: &OutputRow, csv_output: bool) {
    if csv_output {
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(std::io::stdout());

        writer.serialize(row).expect("failed to serialize CSV row");
        writer.flush().expect("failed to flush CSV writer");
    } else {
        println!("{row:#?}");
    }
}

#[allow(clippy::arithmetic_side_effects)]
fn main() {
    let config = Config::parse();
    validate_config(&config).unwrap_or_else(|err| {
        eprintln!("error: {err}");
        std::process::exit(1);
    });

    let mut ctx = init_example_context(config.num_threads, config.num_validators);

    let (batches, bad_packets_generated) = build_all_vote_batches(
        &ctx,
        config.total_packets,
        config.batch_size,
        config.two_slot_mode,
        config.bad_ratio,
        config.seed,
    );

    let start = Instant::now();

    for batch in batches {
        ctx.verifier.verify_and_send_batches_for_tests(vec![batch]);
    }

    let elapsed = start.elapsed();

    let valid_packets_generated = config.total_packets - bad_packets_generated;
    let elapsed_us =
        u64::try_from(elapsed.as_micros()).expect("elapsed microseconds must fit into u64");

    let per_packet_us = elapsed_us
        .checked_div(u64::try_from(config.total_packets).expect("total_packets must fit into u64"))
        .expect("total_packets must be > 0");

    let per_valid_packet_us = if valid_packets_generated > 0 {
        elapsed_us
            .checked_div(
                u64::try_from(valid_packets_generated)
                    .expect("valid_packets_generated must fit into u64"),
            )
            .expect("valid_packets_generated must be > 0")
    } else {
        elapsed_us
    };

    let row = OutputRow {
        seed: config.seed,
        batch_size: config.batch_size,
        total_packets: config.total_packets,
        bad_ratio: config.bad_ratio,
        two_slot_mode: config.two_slot_mode,
        num_threads: config.num_threads,
        num_validators: config.num_validators,
        bad_packets: bad_packets_generated,
        valid_packets: valid_packets_generated,
        elapsed_us,
        per_packet_us,
        per_valid_packet_us,
    };

    print_results(&row, config.csv);
}
