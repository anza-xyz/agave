use {
    crate::{
        cluster_info::{ClusterInfo, GOSSIP_SLEEP_MILLIS},
        cluster_info_metrics::ScopedTimer,
        crds_gossip_pull::CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS,
        epoch_specs::EpochSpecs,
        gossip_command::GossipCommand,
        gossip_context::GossipContext,
        gossip_error::GossipError,
        gossip_ingress::ValidatedGossipMessage,
        gossip_timer::Periodic,
    },
    crossbeam_channel::{Receiver, Sender},
    rayon::ThreadPool,
    solana_perf::packet::{PacketBatch, PacketBatchRecycler},
    solana_pubkey::Pubkey,
    solana_streamer::streamer::ChannelSend,
    std::{
        collections::HashSet,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const TICK_INTERVAL: Duration = Duration::from_millis(GOSSIP_SLEEP_MILLIS);
const PULL_INTERVAL: Duration = Duration::from_millis(GOSSIP_SLEEP_MILLIS * 5);
const PUSH_REFRESH_INTERVAL: Duration = Duration::from_millis(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS / 2);
const MAX_COMMANDS_PER_TURN: usize = 256;
const MAX_INGRESS_BATCHES_PER_TURN: usize = 1024;
const TARGET_INGRESS_MESSAGES_PER_TURN: usize = 64 * 1024;

fn buffer_ingress(
    first: Vec<ValidatedGossipMessage>,
    inbound: &Receiver<Vec<ValidatedGossipMessage>>,
    buffer: &mut Vec<Vec<ValidatedGossipMessage>>,
) {
    let mut num_messages = first.len();
    buffer.push(first);
    while buffer.len() < MAX_INGRESS_BATCHES_PER_TURN
        && num_messages < TARGET_INGRESS_MESSAGES_PER_TURN
    {
        let Ok(messages) = inbound.try_recv() else {
            break;
        };
        num_messages = num_messages.saturating_add(messages.len());
        buffer.push(messages);
    }
}

struct Deadlines {
    tick: Periodic,
    pull: Periodic,
    push_refresh: Periodic,
}

impl Deadlines {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            tick: Periodic::due_now(now, TICK_INTERVAL),
            pull: Periodic::due_now(now, PULL_INTERVAL),
            push_refresh: Periodic::due_now(now, PUSH_REFRESH_INTERVAL),
        }
    }
}

pub(crate) struct GossipEngine<S> {
    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) epoch_specs: Option<Box<dyn EpochSpecs>>,
    pub(crate) workers: Arc<ThreadPool>,
    pub(crate) context: Arc<GossipContext>,
    pub(crate) command_endpoint: Arc<Sender<GossipCommand>>,
    pub(crate) commands: Receiver<GossipCommand>,
    pub(crate) inbound: Receiver<Vec<ValidatedGossipMessage>>,
    pub(crate) outbound: S,
    pub(crate) validators: Option<HashSet<Pubkey>>,
    pub(crate) check_duplicate_instance: bool,
    pub(crate) exit: Arc<AtomicBool>,
}

// Avoid a per-vote allocation.
#[allow(clippy::large_enum_variant)]
enum EngineEvent {
    Command(GossipCommand),
    Packets(Vec<ValidatedGossipMessage>),
    Tick,
    Disconnected,
}

impl<S: ChannelSend<PacketBatch>> GossipEngine<S> {
    pub(crate) fn spawn(self) -> JoinHandle<()> {
        Builder::new()
            .name("solGossipEngine".to_string())
            .spawn(move || {
                let Self {
                    cluster_info,
                    mut epoch_specs,
                    workers,
                    context,
                    command_endpoint,
                    commands,
                    inbound,
                    outbound,
                    validators,
                    check_duplicate_instance,
                    exit,
                } = self;
                let _writer_lease = cluster_info.acquire_writer_lease();
                let recycler = PacketBatchRecycler::default();
                let mut deadlines = Deadlines::new();
                let mut packet_buf = Vec::with_capacity(MAX_INGRESS_BATCHES_PER_TURN);
                let mut entrypoints_processed = false;

                while !exit.load(Ordering::Relaxed) {
                    let timeout = deadlines
                        .tick
                        .deadline()
                        .saturating_duration_since(Instant::now());
                    // Prioritize local CRDS mutations.
                    let event = crossbeam_channel::select_biased! {
                        recv(commands) -> command => {
                            command.map_or(EngineEvent::Disconnected, EngineEvent::Command)
                        },
                        recv(inbound) -> packets => {
                            packets.map_or(EngineEvent::Disconnected, EngineEvent::Packets)
                        },
                        default(timeout) => EngineEvent::Tick,
                    };
                    match event {
                        EngineEvent::Command(command) => {
                            cluster_info.apply_command(command);
                            for command in commands.try_iter().take(MAX_COMMANDS_PER_TURN - 1) {
                                cluster_info.apply_command(command);
                            }
                        }
                        EngineEvent::Packets(packets) => {
                            buffer_ingress(packets, &inbound, &mut packet_buf);
                            let context_snapshot = context.load();
                            let _timer =
                                ScopedTimer::from(&cluster_info.stats.gossip_listen_loop_time);
                            let result = cluster_info.process_packets(
                                &mut packet_buf,
                                &workers,
                                &recycler,
                                &outbound,
                                &context_snapshot.stakes,
                                check_duplicate_instance,
                            );
                            packet_buf.clear();
                            cluster_info
                                .stats
                                .gossip_listen_loop_iterations_since_last_report
                                .add_relaxed(1);
                            if let Err(err) = result {
                                match err {
                                    GossipError::DuplicateNodeInstance => {
                                        error!(
                                            "duplicate running instances of the same validator \
                                             node: {}",
                                            cluster_info.id()
                                        );
                                        exit.store(true, Ordering::Relaxed);
                                        std::process::exit(1);
                                    }
                                    _ => error!("gossip engine failed to process messages: {err}"),
                                }
                            }
                        }
                        EngineEvent::Tick => {}
                        EngineEvent::Disconnected => break,
                    }

                    let now = Instant::now();
                    if !deadlines.tick.claim(now) {
                        continue;
                    }

                    let stakes = epoch_specs
                        .as_mut()
                        .map(|epoch_specs| epoch_specs.current_epoch_staked_nodes())
                        .unwrap_or_default();
                    context.update(Arc::clone(&stakes), cluster_info.is_full_alpenglow_epoch());

                    let generate_pull = deadlines.pull.claim(now);
                    let _ = cluster_info.run_gossip(
                        &workers,
                        validators.as_ref(),
                        &recycler,
                        &stakes,
                        &outbound,
                        generate_pull,
                    );
                    cluster_info.handle_purge(&workers, &stakes);
                    if !entrypoints_processed {
                        entrypoints_processed = cluster_info.process_entrypoints();
                    }

                    if deadlines.push_refresh.claim(now) {
                        cluster_info.refresh_my_gossip_contact_info();
                        cluster_info.refresh_push_active_set(
                            &recycler,
                            &stakes,
                            validators.as_ref(),
                            &outbound,
                        );
                    }
                }
                cluster_info.detach_command_endpoint(&command_endpoint);
                drop(command_endpoint);
                for command in commands {
                    cluster_info.apply_command(command);
                }
            })
            .unwrap()
    }
}
