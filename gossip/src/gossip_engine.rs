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

/// State the engine owns for the lifetime of the thread, kept out of
/// [`GossipEngine`] so the caller only supplies dependencies.
struct LoopState {
    recycler: PacketBatchRecycler,
    deadlines: Deadlines,
    packet_buf: Vec<Vec<ValidatedGossipMessage>>,
    entrypoints_processed: bool,
}

impl LoopState {
    fn new() -> Self {
        Self {
            recycler: PacketBatchRecycler::default(),
            deadlines: Deadlines::new(),
            packet_buf: Vec::with_capacity(MAX_INGRESS_BATCHES_PER_TURN),
            entrypoints_processed: false,
        }
    }
}

impl<S: ChannelSend<PacketBatch>> GossipEngine<S> {
    pub(crate) fn spawn(self) -> JoinHandle<()> {
        Builder::new()
            .name("solGossipEngine".to_string())
            .spawn(move || self.run())
            .unwrap()
    }

    fn run(mut self) {
        // Lease a cloned handle: `self` is moved into `drain_commands` while
        // the lease is still held.
        let cluster_info = Arc::clone(&self.cluster_info);
        let _writer_lease = cluster_info.acquire_writer_lease();
        let mut state = LoopState::new();
        while !self.exit.load(Ordering::Relaxed) {
            match self.next_event(state.deadlines.tick.deadline) {
                EngineEvent::Command(command) => self.apply_commands(command),
                EngineEvent::Packets(packets) => self.process_ingress(packets, &mut state),
                EngineEvent::Tick => (),
                EngineEvent::Disconnected => break,
            }
            let now = Instant::now();
            if state.deadlines.tick.claim(now) {
                self.run_gossip_round(now, &mut state);
            }
        }
        self.drain_commands();
    }

    /// Blocks for a command, a batch of validated packets, or the tick deadline.
    fn next_event(&self, tick_deadline: Instant) -> EngineEvent {
        let timeout = tick_deadline.saturating_duration_since(Instant::now());
        let commands = &self.commands;
        let inbound = &self.inbound;
        // Prioritize local CRDS mutations.
        crossbeam_channel::select_biased! {
            recv(commands) -> command => {
                command.map_or(EngineEvent::Disconnected, EngineEvent::Command)
            },
            recv(inbound) -> packets => {
                packets.map_or(EngineEvent::Disconnected, EngineEvent::Packets)
            },
            default(timeout) => EngineEvent::Tick,
        }
    }

    /// Applies `first` and whatever else is already queued behind it.
    fn apply_commands(&self, first: GossipCommand) {
        self.cluster_info.apply_command(first);
        for command in self.commands.try_iter().take(MAX_COMMANDS_PER_TURN - 1) {
            self.cluster_info.apply_command(command);
        }
    }

    /// Merges `packets` with whatever else has arrived and processes the batch.
    fn process_ingress(&self, packets: Vec<ValidatedGossipMessage>, state: &mut LoopState) {
        buffer_ingress(packets, &self.inbound, &mut state.packet_buf);
        let context_snapshot = self.context.load();
        let _timer = ScopedTimer::from(&self.cluster_info.stats.gossip_listen_loop_time);
        let result = self.cluster_info.process_packets(
            &mut state.packet_buf,
            &self.workers,
            &state.recycler,
            &self.outbound,
            &context_snapshot.stakes,
            self.check_duplicate_instance,
        );
        state.packet_buf.clear();
        self.cluster_info
            .stats
            .gossip_listen_loop_iterations_since_last_report
            .add_relaxed(1);
        match result {
            Ok(()) => (),
            Err(GossipError::DuplicateNodeInstance) => {
                error!(
                    "duplicate running instances of the same validator node: {}",
                    self.cluster_info.id()
                );
                self.exit.store(true, Ordering::Relaxed);
                std::process::exit(1);
            }
            Err(err) => error!("gossip engine failed to process messages: {err}"),
        }
    }

    /// One gossip round: refresh the policy snapshot, push and pull, purge, and
    /// periodically refresh the active set.
    fn run_gossip_round(&mut self, now: Instant, state: &mut LoopState) {
        let stakes = self
            .epoch_specs
            .as_mut()
            .map(|epoch_specs| epoch_specs.current_epoch_staked_nodes())
            .unwrap_or_default();
        self.context.update(
            Arc::clone(&stakes),
            self.cluster_info.is_full_alpenglow_epoch(),
        );

        let generate_pull = state.deadlines.pull.claim(now);
        let _ = self.cluster_info.run_gossip(
            &self.workers,
            self.validators.as_ref(),
            &state.recycler,
            &stakes,
            &self.outbound,
            generate_pull,
        );
        self.cluster_info.handle_purge(&self.workers, &stakes);
        if !state.entrypoints_processed {
            state.entrypoints_processed = self.cluster_info.process_entrypoints();
        }

        if state.deadlines.push_refresh.claim(now) {
            self.cluster_info.refresh_my_gossip_contact_info();
            self.cluster_info.refresh_push_active_set(
                &state.recycler,
                &stakes,
                self.validators.as_ref(),
                &self.outbound,
            );
        }
    }

    /// Detaches the endpoint and applies whatever is still queued, so callers
    /// racing shutdown are never left waiting on a reply.
    fn drain_commands(self) {
        self.cluster_info
            .detach_command_endpoint(&self.command_endpoint);
        drop(self.command_endpoint);
        for command in self.commands {
            self.cluster_info.apply_command(command);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::contact_info::ContactInfo, crossbeam_channel::bounded,
        rayon::ThreadPoolBuilder, solana_keypair::Keypair, solana_net_utils::SocketAddrSpace,
        solana_signer::Signer, std::collections::HashMap,
    };

    #[test]
    fn drain_commands_applies_queued_commands() {
        let keypair = Arc::new(Keypair::new());
        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&keypair.pubkey(), 0),
            keypair,
            SocketAddrSpace::Unspecified,
        ));
        let (command_sender, commands) = bounded(1);
        let command_endpoint = cluster_info.attach_command_endpoint(command_sender);
        command_endpoint
            .send(GossipCommand::LowestSlot(42))
            .unwrap();
        let (_inbound_sender, inbound) = bounded(1);
        let (outbound, _outbound_receiver) = bounded(1);
        let engine = GossipEngine {
            cluster_info: Arc::clone(&cluster_info),
            epoch_specs: None,
            workers: Arc::new(ThreadPoolBuilder::new().num_threads(1).build().unwrap()),
            context: Arc::new(GossipContext::new(Arc::new(HashMap::new()), false)),
            command_endpoint,
            commands,
            inbound,
            outbound,
            validators: None,
            check_duplicate_instance: false,
            exit: Arc::new(AtomicBool::new(false)),
        };

        engine.drain_commands();

        assert_eq!(
            cluster_info
                .lowest_slot_for_tests(cluster_info.id())
                .unwrap()
                .lowest,
            42
        );
    }
}
