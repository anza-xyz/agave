use {
    crate::{
        cluster_info::{CHANNEL_CONSUME_CAPACITY, ClusterInfo, GOSSIP_SLEEP_MILLIS},
        cluster_info_metrics::ScopedTimer,
        crds_gossip_pull::CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS,
        epoch_specs::EpochSpecs,
        gossip_command::{GOSSIP_COMMAND_CAPACITY, GossipCommand},
        gossip_context::GossipContext,
        gossip_error::GossipError,
        gossip_ingress::ValidatedGossipMessage,
    },
    crossbeam_channel::{Receiver, Sender},
    rayon::ThreadPool,
    solana_perf::packet::{PacketBatch, PacketBatchRecycler},
    solana_pubkey::Pubkey,
    solana_streamer::streamer::{ChannelSend, StreamerReceiveStats},
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
const METRICS_INTERVAL: Duration = Duration::from_secs(2);

struct Periodic {
    deadline: Instant,
    period: Duration,
}

impl Periodic {
    fn due_now(now: Instant, period: Duration) -> Self {
        Self {
            deadline: now,
            period,
        }
    }

    fn due_after(now: Instant, period: Duration) -> Self {
        Self {
            deadline: now + period,
            period,
        }
    }

    fn claim(&mut self, now: Instant) -> bool {
        if now < self.deadline {
            return false;
        }
        self.deadline = now + self.period;
        true
    }
}

struct Deadlines {
    tick: Periodic,
    pull: Periodic,
    push_refresh: Periodic,
    metrics: Periodic,
    contact_trace: Option<Periodic>,
    contact_save: Option<Periodic>,
}

impl Deadlines {
    fn new(cluster_info: &ClusterInfo) -> Self {
        let now = Instant::now();
        Self {
            tick: Periodic::due_now(now, TICK_INTERVAL),
            pull: Periodic::due_now(now, PULL_INTERVAL),
            push_refresh: Periodic::due_now(now, PUSH_REFRESH_INTERVAL),
            metrics: Periodic::due_after(now, METRICS_INTERVAL),
            contact_trace: cluster_info
                .contact_debug_interval()
                .map(|period| Periodic::due_after(now, period)),
            contact_save: cluster_info
                .contact_save_interval()
                .map(|period| Periodic::due_after(now, period)),
        }
    }

    fn claim(periodic: &mut Option<Periodic>, now: Instant) -> bool {
        periodic
            .as_mut()
            .is_some_and(|periodic| periodic.claim(now))
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
    pub(crate) receiver_stats: Arc<StreamerReceiveStats>,
    pub(crate) validators: Option<HashSet<Pubkey>>,
    pub(crate) check_duplicate_instance: bool,
    pub(crate) exit: Arc<AtomicBool>,
}

// Boxing commands here would allocate on every engine-bound vote merely to
// shrink this short-lived stack value.
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
                    receiver_stats,
                    validators,
                    check_duplicate_instance,
                    exit,
                } = self;
                let recycler = PacketBatchRecycler::default();
                let mut deadlines = Deadlines::new(&cluster_info);
                let mut packet_buf = Vec::with_capacity(CHANNEL_CONSUME_CAPACITY);
                let mut entrypoints_processed = false;

                while !exit.load(Ordering::Relaxed) {
                    let timeout = deadlines
                        .tick
                        .deadline
                        .saturating_duration_since(Instant::now());
                    // Commands are preferred over packets: they are the only
                    // way local services reach the CRDS table.
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
                            cluster_info.process_command(command);
                            for command in commands.try_iter().take(GOSSIP_COMMAND_CAPACITY - 1) {
                                cluster_info.process_command(command);
                            }
                        }
                        EngineEvent::Packets(packets) => {
                            packet_buf.push(packets);
                            packet_buf
                                .extend(inbound.try_iter().take(CHANNEL_CONSUME_CAPACITY - 1));
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

                    if deadlines.metrics.claim(now) {
                        cluster_info.submit_stats(&stakes);
                        receiver_stats.report();
                    }

                    if Deadlines::claim(&mut deadlines.contact_trace, now) {
                        info!(
                            "\n{}\n\n{}",
                            cluster_info.contact_info_trace(),
                            cluster_info.rpc_info_trace()
                        );
                    }
                    if Deadlines::claim(&mut deadlines.contact_save, now) {
                        cluster_info.save_contact_info();
                    }

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
                cluster_info.clear_command_sender(&command_endpoint);
            })
            .unwrap()
    }
}
