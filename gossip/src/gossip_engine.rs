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

const PULL_INTERVAL: Duration = Duration::from_millis(GOSSIP_SLEEP_MILLIS * 5);
const PUSH_REFRESH_INTERVAL: Duration = Duration::from_millis(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS / 2);
const METRICS_INTERVAL: Duration = Duration::from_secs(2);

struct Deadlines {
    tick: Instant,
    pull: Instant,
    push_refresh: Instant,
    metrics: Instant,
    contact_trace: Option<Instant>,
    contact_save: Option<Instant>,
}

impl Deadlines {
    fn new(cluster_info: &ClusterInfo) -> Self {
        let now = Instant::now();
        Self {
            tick: now,
            pull: now,
            push_refresh: now,
            metrics: now + METRICS_INTERVAL,
            contact_trace: cluster_info
                .contact_debug_interval()
                .map(|period| now + period),
            contact_save: cluster_info
                .contact_save_interval()
                .map(|period| now + period),
        }
    }

    fn claim(deadline: &mut Instant, now: Instant, period: Duration) -> bool {
        if now < *deadline {
            return false;
        }
        *deadline = now + period;
        true
    }

    fn claim_optional(deadline: &mut Option<Instant>, now: Instant, period: Duration) -> bool {
        deadline
            .as_mut()
            .is_some_and(|deadline| Self::claim(deadline, now, period))
    }
}

pub(crate) struct GossipEngine;

// Boxing commands here would allocate on every engine-bound vote merely to
// shrink this short-lived stack value.
#[allow(clippy::large_enum_variant)]
enum EngineEvent {
    Command(GossipCommand),
    Packets(Vec<ValidatedGossipMessage>),
    Tick,
    Disconnected,
}

impl GossipEngine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        cluster_info: Arc<ClusterInfo>,
        mut epoch_specs: Option<Box<dyn EpochSpecs>>,
        thread_pool: Arc<ThreadPool>,
        context: Arc<GossipContext>,
        command_sender: Arc<Sender<GossipCommand>>,
        command_receiver: Receiver<GossipCommand>,
        receiver: Receiver<Vec<ValidatedGossipMessage>>,
        sender: impl ChannelSend<PacketBatch>,
        receiver_stats: Arc<StreamerReceiveStats>,
        gossip_validators: Option<HashSet<Pubkey>>,
        should_check_duplicate_instance: bool,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solGossipEngine".to_string())
            .spawn(move || {
                let recycler = PacketBatchRecycler::default();
                let mut deadlines = Deadlines::new(&cluster_info);
                let mut packet_buf = Vec::with_capacity(1024);
                let mut entrypoints_processed = false;

                while !exit.load(Ordering::Relaxed) {
                    let timeout = deadlines.tick.saturating_duration_since(Instant::now());
                    let event = crossbeam_channel::select_biased! {
                        recv(command_receiver) -> command => {
                            command.map_or(EngineEvent::Disconnected, EngineEvent::Command)
                        },
                        recv(receiver) -> packets => {
                            packets.map_or(EngineEvent::Disconnected, EngineEvent::Packets)
                        },
                        default(timeout) => EngineEvent::Tick,
                    };
                    match event {
                        EngineEvent::Command(command) => {
                            cluster_info.process_command(command);
                            for command in command_receiver.try_iter().take(1024 - 1) {
                                cluster_info.process_command(command);
                            }
                        }
                        EngineEvent::Packets(packets) => {
                            packet_buf.push(packets);
                            packet_buf.extend(receiver.try_iter().take(1024 - 1));
                            let context_snapshot = context.load();
                            let _timer =
                                ScopedTimer::from(&cluster_info.stats.gossip_listen_loop_time);
                            let result = cluster_info.process_packets(
                                &mut packet_buf,
                                &thread_pool,
                                &recycler,
                                &sender,
                                &context_snapshot.stakes,
                                should_check_duplicate_instance,
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
                    if now < deadlines.tick {
                        continue;
                    }
                    deadlines.tick = now + Duration::from_millis(GOSSIP_SLEEP_MILLIS);

                    let stakes = epoch_specs
                        .as_mut()
                        .map(|epoch_specs| epoch_specs.current_epoch_staked_nodes())
                        .unwrap_or_default();
                    context.update(Arc::clone(&stakes), cluster_info.is_full_alpenglow_epoch());

                    if Deadlines::claim(&mut deadlines.metrics, now, METRICS_INTERVAL) {
                        cluster_info.submit_stats(&stakes);
                        receiver_stats.report();
                    }

                    if let Some(period) = cluster_info.contact_debug_interval()
                        && Deadlines::claim_optional(&mut deadlines.contact_trace, now, period)
                    {
                        info!(
                            "\n{}\n\n{}",
                            cluster_info.contact_info_trace(),
                            cluster_info.rpc_info_trace()
                        );
                    }
                    if let Some(period) = cluster_info.contact_save_interval()
                        && Deadlines::claim_optional(&mut deadlines.contact_save, now, period)
                    {
                        cluster_info.save_contact_info();
                    }

                    let generate_pull = Deadlines::claim(&mut deadlines.pull, now, PULL_INTERVAL);
                    let _ = cluster_info.run_gossip(
                        &thread_pool,
                        gossip_validators.as_ref(),
                        &recycler,
                        &stakes,
                        &sender,
                        generate_pull,
                    );
                    cluster_info.handle_purge(&thread_pool, &stakes);
                    if !entrypoints_processed {
                        entrypoints_processed = cluster_info.process_entrypoints();
                    }

                    if Deadlines::claim(&mut deadlines.push_refresh, now, PUSH_REFRESH_INTERVAL) {
                        cluster_info.refresh_my_gossip_contact_info();
                        cluster_info.refresh_push_active_set(
                            &recycler,
                            &stakes,
                            gossip_validators.as_ref(),
                            &sender,
                        );
                    }
                }
                cluster_info.clear_command_sender(&command_sender);
            })
            .unwrap()
    }
}
