use {
    crate::{
        cluster_info::GOSSIP_SLEEP_MILLIS, cluster_info_metrics::ScopedTimer,
        crds_gossip_pull::CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS, engine_cluster_info::EngineClusterInfo,
        epoch_specs::EpochSpecs, gossip_command::GossipCommand, gossip_error::GossipError,
        gossip_ingress::ValidatedGossipMessage, gossip_policy::GossipPolicy,
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

const GOSSIP_ROUND_INTERVAL: Duration = Duration::from_millis(GOSSIP_SLEEP_MILLIS);
const PULL_REQUEST_INTERVAL: Duration = Duration::from_millis(GOSSIP_SLEEP_MILLIS * 5);
const PUSH_STATE_REFRESH_INTERVAL: Duration =
    Duration::from_millis(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS / 2);
const MAX_COMMANDS_PER_TURN: usize = 256;
const MAX_INGRESS_BATCHES_PER_TURN: usize = 1024;
const TARGET_INGRESS_MESSAGES_PER_TURN: usize = 64 * 1024;

fn buffer_ingress(
    first_batch: Vec<ValidatedGossipMessage>,
    ingress_receiver: &Receiver<Vec<ValidatedGossipMessage>>,
    message_batches: &mut Vec<Vec<ValidatedGossipMessage>>,
) {
    let mut num_messages = first_batch.len();
    message_batches.push(first_batch);
    while message_batches.len() < MAX_INGRESS_BATCHES_PER_TURN
        && num_messages < TARGET_INGRESS_MESSAGES_PER_TURN
    {
        let Ok(messages) = ingress_receiver.try_recv() else {
            break;
        };
        num_messages = num_messages.saturating_add(messages.len());
        message_batches.push(messages);
    }
}

struct Deadlines {
    gossip_round: Periodic,
    pull_request: Periodic,
    push_state_refresh: Periodic,
}

impl Deadlines {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            gossip_round: Periodic::due_now(now, GOSSIP_ROUND_INTERVAL),
            pull_request: Periodic::due_now(now, PULL_REQUEST_INTERVAL),
            push_state_refresh: Periodic::due_now(now, PUSH_STATE_REFRESH_INTERVAL),
        }
    }
}

pub(crate) struct GossipEngine<S> {
    pub(crate) cluster_info: EngineClusterInfo,
    pub(crate) epoch_specs: Option<Box<dyn EpochSpecs>>,
    pub(crate) thread_pool: Arc<ThreadPool>,
    pub(crate) policy: Arc<GossipPolicy>,
    pub(crate) command_sender: Arc<Sender<GossipCommand>>,
    pub(crate) command_receiver: Receiver<GossipCommand>,
    pub(crate) ingress_receiver: Receiver<Vec<ValidatedGossipMessage>>,
    pub(crate) outbound_sender: S,
    pub(crate) gossip_validators: Option<HashSet<Pubkey>>,
    pub(crate) should_check_duplicate_instance: bool,
    pub(crate) exit: Arc<AtomicBool>,
}

// Avoid a per-vote allocation.
#[allow(clippy::large_enum_variant)]
enum EngineEvent {
    Command(GossipCommand),
    Ingress(Vec<ValidatedGossipMessage>),
    GossipRoundDue,
    Disconnected,
}

/// State the engine owns for the lifetime of the thread, kept out of
/// [`GossipEngine`] so the caller only supplies dependencies.
struct LoopState {
    recycler: PacketBatchRecycler,
    deadlines: Deadlines,
    message_batches: Vec<Vec<ValidatedGossipMessage>>,
    entrypoints_processed: bool,
}

impl LoopState {
    fn new() -> Self {
        Self {
            recycler: PacketBatchRecycler::default(),
            deadlines: Deadlines::new(),
            message_batches: Vec::with_capacity(MAX_INGRESS_BATCHES_PER_TURN),
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
        // Lease through a separate handle: `self` is moved into
        // `drain_commands` while the lease is still held.
        let cluster_info = self.cluster_info.clone();
        let _writer_lease = cluster_info.acquire_writer_lease();
        let mut state = LoopState::new();
        while !self.exit.load(Ordering::Relaxed) {
            match self.next_event(state.deadlines.gossip_round.deadline) {
                EngineEvent::Command(command) => self.apply_commands(command),
                EngineEvent::Ingress(messages) => self.process_ingress(messages, &mut state),
                EngineEvent::GossipRoundDue => (),
                EngineEvent::Disconnected => break,
            }
            let now = Instant::now();
            if state.deadlines.gossip_round.claim_due(now) {
                self.run_gossip_round(now, &mut state);
            }
        }
        self.drain_commands();
    }

    /// Blocks for a command, validated messages, or the next gossip round.
    fn next_event(&self, gossip_round_deadline: Instant) -> EngineEvent {
        let timeout = gossip_round_deadline.saturating_duration_since(Instant::now());
        let command_receiver = &self.command_receiver;
        let ingress_receiver = &self.ingress_receiver;
        // Prioritize local CRDS mutations.
        crossbeam_channel::select_biased! {
            recv(command_receiver) -> command => {
                command.map_or(EngineEvent::Disconnected, EngineEvent::Command)
            },
            recv(ingress_receiver) -> messages => {
                messages.map_or(EngineEvent::Disconnected, EngineEvent::Ingress)
            },
            default(timeout) => EngineEvent::GossipRoundDue,
        }
    }

    /// Applies `first` and whatever else is already queued behind it.
    fn apply_commands(&self, first: GossipCommand) {
        self.cluster_info.apply_command(first);
        for command in self
            .command_receiver
            .try_iter()
            .take(MAX_COMMANDS_PER_TURN - 1)
        {
            self.cluster_info.apply_command(command);
        }
    }

    /// Merges `messages` with whatever else has arrived and processes the batch.
    fn process_ingress(&self, messages: Vec<ValidatedGossipMessage>, state: &mut LoopState) {
        buffer_ingress(messages, &self.ingress_receiver, &mut state.message_batches);
        let policy_snapshot = self.policy.load();
        let _timer = ScopedTimer::from(&self.cluster_info.stats().gossip_listen_loop_time);
        let result = self.cluster_info.handle_validated_messages(
            &mut state.message_batches,
            &self.thread_pool,
            &state.recycler,
            &self.outbound_sender,
            &policy_snapshot.stakes,
            self.should_check_duplicate_instance,
        );
        state.message_batches.clear();
        self.cluster_info
            .stats()
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
        self.policy.update(
            Arc::clone(&stakes),
            self.cluster_info.is_full_alpenglow_epoch(),
        );

        let generate_pull_requests = state.deadlines.pull_request.claim_due(now);
        let _ = self.cluster_info.send_gossip_requests(
            &self.thread_pool,
            self.gossip_validators.as_ref(),
            &state.recycler,
            &stakes,
            &self.outbound_sender,
            generate_pull_requests,
        );
        self.cluster_info
            .purge_expired_crds(&self.thread_pool, &stakes);
        if !state.entrypoints_processed {
            state.entrypoints_processed = self.cluster_info.process_entrypoints();
        }

        if state.deadlines.push_state_refresh.claim_due(now) {
            self.cluster_info.refresh_my_gossip_contact_info();
            self.cluster_info.refresh_push_active_set(
                &state.recycler,
                &stakes,
                self.gossip_validators.as_ref(),
                &self.outbound_sender,
            );
        }
    }

    /// Detaches the endpoint and applies whatever is still queued, so callers
    /// racing shutdown are never left waiting on a reply.
    fn drain_commands(self) {
        self.cluster_info
            .detach_command_endpoint(&self.command_sender);
        drop(self.command_sender);
        for command in self.command_receiver {
            self.cluster_info.apply_command(command);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        crossbeam_channel::bounded,
        rayon::ThreadPoolBuilder,
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_signer::Signer,
        std::collections::HashMap,
    };

    #[test]
    fn drain_commands_applies_queued_commands() {
        let keypair = Arc::new(Keypair::new());
        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&keypair.pubkey(), 0),
            keypair,
            SocketAddrSpace::Unspecified,
        ));
        let (command_sender, command_receiver) = bounded(1);
        let command_endpoint = cluster_info.attach_command_endpoint(command_sender);
        command_endpoint
            .send(GossipCommand::PublishLowestSlot(42))
            .unwrap();
        let (_validated_sender, validated_receiver) = bounded(1);
        let (outbound_sender, _outbound_receiver) = bounded(1);
        let engine = GossipEngine {
            cluster_info: EngineClusterInfo::new(Arc::clone(&cluster_info)),
            epoch_specs: None,
            thread_pool: Arc::new(ThreadPoolBuilder::new().num_threads(1).build().unwrap()),
            policy: Arc::new(GossipPolicy::new(Arc::new(HashMap::new()), false)),
            command_sender: command_endpoint,
            command_receiver,
            ingress_receiver: validated_receiver,
            outbound_sender,
            gossip_validators: None,
            should_check_duplicate_instance: false,
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
