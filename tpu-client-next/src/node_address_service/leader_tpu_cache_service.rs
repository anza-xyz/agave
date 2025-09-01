use {
    crate::node_address_service::NodeAddressServiceError,
    log::*,
    solana_clock::{Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_pubkey::Pubkey,
    solana_quic_definitions::QUIC_PORT_OFFSET,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::response::RpcContactInfo,
    solana_time_utils::timestamp,
    std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc},
    tokio::{
        sync::watch,
        time::{Duration, Instant},
    },
    tokio_util::sync::CancellationToken,
};

/// Maximum number of slots used to build TPU socket fanout set
const MAX_FANOUT_SLOTS: u64 = 100;

/// Configuration for the [`LeaderTpuCacheService`].
#[derive(Debug, Clone)]
pub struct LeaderTpuCacheServiceConfig {
    lookahead_leaders: u64,
    refresh_every: Duration,
    max_consecutive_failures: usize,
}

/// [`LeaderTpuCacheService`] is a background task that tracks the current and
/// upcoming Solana leader nodes and updates their TPU socket addresses in a
/// watch channel for downstream consumers.
pub struct LeaderTpuCacheService {
    rpc_client: Arc<RpcClient>,
    config: LeaderTpuCacheServiceConfig,
    slot_receiver: watch::Receiver<(Slot, u64, Duration)>,
    leaders_sender: watch::Sender<(Slot, Vec<SocketAddr>)>,
    cancel: CancellationToken,
}

impl LeaderTpuCacheService {
    pub async fn new(
        rpc_client: Arc<RpcClient>,
        slot_receiver: watch::Receiver<(Slot, u64, Duration)>,
        leaders_sender: watch::Sender<(Slot, Vec<SocketAddr>)>,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<Self, NodeAddressServiceError> {
        Ok(Self {
            rpc_client,
            config,
            slot_receiver,
            leaders_sender,
            cancel,
        })
    }

    pub async fn run(self) -> Result<(), NodeAddressServiceError> {
        let Self {
            rpc_client,
            config,
            mut slot_receiver,
            leaders_sender,
            cancel,
        } = self;
        let lookahead_slots = config
            .lookahead_leaders
            .saturating_mul(NUM_CONSECUTIVE_LEADER_SLOTS);

        let mut leader_tpu_cache = LeaderTpuCacheServiceState::new(0, 0, 0, vec![], vec![]);

        let mut num_consequent_failures: usize = 0;
        let mut last_cluster_refresh = Instant::now() - config.refresh_every; // to ensure we refresh immediately
        loop {
            tokio::select! {
                res = slot_receiver.changed() => {
                    if let Err(e) = res {
                        warn!("Slot receiver channel closed: {e}");
                        break;
                    }

                    debug!("Slot update received, refreshing leader cache: {:?}", last_cluster_refresh.elapsed());
                    if let Err(e) = leader_tpu_cache.update(
                        &mut last_cluster_refresh,
                        config.refresh_every,
                        &rpc_client,
                        &slot_receiver
                    ).await {
                        debug!("Failed to update leader cache: {e}");
                        num_consequent_failures = num_consequent_failures.saturating_add(1);
                        if num_consequent_failures >= config.max_consecutive_failures {
                            error!("Failed to update leader cache {} times, giving up", num_consequent_failures);
                            return Err(e);
                        }
                    } else {
                        debug!("Leader cache updated successfully");
                        num_consequent_failures = 0;
                    }

                    let current_slot = slot_receiver.borrow().0;
                    let leaders = leader_tpu_cache.get_leader_sockets(
                        current_slot, lookahead_slots);
                    if let Err(e) = leaders_sender.send((current_slot, leaders)) {
                        warn!("Unexpectly dropped leaders_sender: {e}");
                        break;
                    }
                }

                _ = cancel.cancelled() => {
                    info!("Cancel signal received, stopping LeaderTpuCacheService.");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[derive(PartialEq, Debug)]
struct LeaderTpuCacheServiceState {
    first_slot: Slot,
    leaders: Vec<Pubkey>,
    leader_tpu_map: HashMap<Pubkey, SocketAddr>,
    slots_in_epoch: Slot,
    last_slot_in_epoch: Slot,
}

impl LeaderTpuCacheServiceState {
    pub fn new(
        first_slot: Slot,
        slots_in_epoch: Slot,
        last_slot_in_epoch: Slot,
        leaders: Vec<Pubkey>,
        cluster_nodes: Vec<RpcContactInfo>,
    ) -> Self {
        let leader_tpu_map = Self::extract_cluster_tpu_sockets(cluster_nodes);
        Self {
            first_slot,
            leaders,
            leader_tpu_map,
            slots_in_epoch,
            last_slot_in_epoch,
        }
    }

    // Last slot that has a cached leader pubkey
    pub fn last_slot(&self) -> Slot {
        self.first_slot + self.leaders.len().saturating_sub(1) as u64
    }

    pub fn slot_info(&self) -> (Slot, Slot, Slot) {
        (
            self.last_slot(),
            self.last_slot_in_epoch,
            self.slots_in_epoch,
        )
    }

    // Get the TPU sockets for the current leader and upcoming leaders according
    // to fanout size.
    fn get_leader_sockets(
        &self,
        estimated_current_slot: Slot,
        fanout_slots: u64,
    ) -> Vec<SocketAddr> {
        let mut leader_sockets =
            Vec::with_capacity(fanout_slots.div_ceil(NUM_CONSECUTIVE_LEADER_SLOTS) as usize);
        // `first_slot` might have been advanced since caller last read the
        // `estimated_current_slot` value. Take the greater of the two values to
        // ensure we are reading from the latest leader schedule.
        let current_slot = std::cmp::max(estimated_current_slot, self.first_slot);
        for leader_slot in (current_slot..current_slot + fanout_slots)
            .step_by(NUM_CONSECUTIVE_LEADER_SLOTS as usize)
        {
            if let Some(leader) = self.get_slot_leader(leader_slot) {
                if let Some(tpu_socket) = self.leader_tpu_map.get(leader) {
                    leader_sockets.push(*tpu_socket);
                    debug!("Pushed leader {leader} TPU socket: {tpu_socket}");
                } else {
                    // The leader is probably delinquent
                    debug!("TPU not available for leader {leader}");
                }
            } else {
                // Overran the local leader schedule cache
                warn!(
                    "Leader not known for slot {}; cache holds slots [{},{}]",
                    leader_slot,
                    self.first_slot,
                    self.last_slot()
                );
            }
        }
        leader_sockets
    }

    pub fn get_slot_leader(&self, slot: Slot) -> Option<&Pubkey> {
        if slot >= self.first_slot {
            let index = slot - self.first_slot;
            self.leaders.get(index as usize)
        } else {
            None
        }
    }

    pub fn fanout(slots_in_epoch: Slot) -> Slot {
        (2 * MAX_FANOUT_SLOTS).min(slots_in_epoch)
    }

    fn extract_cluster_tpu_sockets(
        cluster_contact_info: Vec<RpcContactInfo>,
    ) -> HashMap<Pubkey, SocketAddr> {
        cluster_contact_info
            .into_iter()
            .filter_map(|contact_info| {
                let pubkey = Pubkey::from_str(&contact_info.pubkey).ok()?;
                let socket = {
                    contact_info.tpu_quic.or_else(|| {
                        let mut socket = contact_info.tpu?;
                        let port = socket.port().checked_add(QUIC_PORT_OFFSET)?;
                        socket.set_port(port);
                        Some(socket)
                    })
                }?;
                Some((pubkey, socket))
            })
            .collect()
    }

    /// Update the leader cache with the latest slot leaders and cluster TPU
    /// ports.
    async fn update(
        &mut self,
        last_cluster_refresh: &mut Instant,
        refresh_nodes_every: Duration,
        rpc_client: &RpcClient,
        slot_receiver: &watch::Receiver<(Slot, u64, Duration)>,
    ) -> Result<(), NodeAddressServiceError> {
        // even if some intermediate step fails, we still want to update state
        // at least partially.
        let mut result = Ok(());
        // Refresh cluster TPU ports every `refresh_every` in case validators restart with
        // new port configuration or new validators come online
        debug!(
            "Refreshing cluster TPU sockets every {:?}, {:?}",
            last_cluster_refresh.elapsed(),
            refresh_nodes_every
        );
        if last_cluster_refresh.elapsed() > refresh_nodes_every {
            let cluster_nodes = rpc_client.get_cluster_nodes().await;
            match cluster_nodes {
                Ok(cluster_nodes) => {
                    self.leader_tpu_map = Self::extract_cluster_tpu_sockets(cluster_nodes);
                    *last_cluster_refresh = Instant::now();
                }
                Err(err) => {
                    warn!("Failed to fetch cluster tpu sockets: {err}");
                    result = Err(NodeAddressServiceError::RpcError(err));
                }
            }
        }

        let (mut estimated_current_slot, start_time, estimated_slot_duration) =
            *slot_receiver.borrow();

        // If we are close to the end of slot, increment the estimated current slot
        if timestamp() - start_time >= estimated_slot_duration.as_millis() as u64 {
            estimated_current_slot = estimated_current_slot.saturating_add(1);
        }

        let (last_slot, last_slot_in_epoch, slots_in_epoch) = self.slot_info();

        debug!(
            "Estimated current slot: {}, last slot: {}, last slot in epoch: {}, slots in epoch: {}",
            estimated_current_slot, last_slot, last_slot_in_epoch, slots_in_epoch
        );
        // If we're crossing into a new epoch, fetch the updated epoch schedule.
        if estimated_current_slot > last_slot_in_epoch {
            debug!(
                "Crossing into a new epoch, fetching updated epoch schedule. \
                 Last slot in epoch: {}, estimated current slot: {}",
                last_slot_in_epoch, estimated_current_slot
            );
            match rpc_client.get_epoch_schedule().await {
                Ok(epoch_schedule) => {
                    let epoch = epoch_schedule.get_epoch(estimated_current_slot);
                    self.slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
                    self.last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);
                }
                Err(err) => {
                    warn!("Failed to fetch epoch schedule: {err}");
                    result = Err(NodeAddressServiceError::RpcError(err));
                }
            }
        }

        // If we are within the fanout range of the last slot in the cache,
        // fetch more slot leaders. We pull down a big batch at at time to
        // amortize the cost of the RPC call. We don't want to stall
        // transactions on pulling this down so we fetch it proactively.
        if estimated_current_slot >= last_slot.saturating_sub(MAX_FANOUT_SLOTS) {
            let slot_leaders = rpc_client
                .get_slot_leaders(
                    estimated_current_slot,
                    LeaderTpuCacheServiceState::fanout(slots_in_epoch),
                )
                .await;
            match slot_leaders {
                Ok(slot_leaders) => {
                    self.first_slot = estimated_current_slot;
                    self.leaders = slot_leaders;
                }
                Err(err) => {
                    warn!(
                        "Failed to fetch slot leaders (first_slot: \
                         {}): {err}",
                        estimated_current_slot
                    );
                    result = Err(NodeAddressServiceError::RpcError(err));
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        async_trait::async_trait,
        serde_json::{json, Value},
        solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
        solana_commitment_config::CommitmentConfig,
        solana_epoch_schedule::EpochSchedule,
        solana_rpc_client::{
            rpc_client::RpcClientConfig,
            rpc_sender::{RpcSender, RpcTransportStats},
        },
        solana_rpc_client_api::{
            client_error,
            request::{self, RpcRequest},
        },
        std::{
            net::{IpAddr, Ipv4Addr},
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc, RwLock,
            },
        },
        tokio::{
            sync::mpsc,
            task::JoinHandle,
            time::{sleep, timeout},
        },
    };

    #[derive(Clone, Debug)]
    enum Step {
        Ok,
        Fail,
        Null,
    }

    #[derive(Debug)]
    struct Plan {
        steps: Vec<Step>,
        cursor: AtomicUsize,
    }
    impl Plan {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps,
                cursor: AtomicUsize::new(0),
            }
        }
        fn next(&self) -> Step {
            let len = self.steps.len();
            if len == 0 {
                return Step::Ok;
            }
            let i = self
                .cursor
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                % len;
            self.steps[i].clone()
        }
    }

    #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
    enum ClusterStateUpdate {
        IncrementSlot(u64),
        ChangeOrAddNode { id: Pubkey, addr: SocketAddr },
        RemoveNode { id: Pubkey },
        // it is present in schedule but not in gossip
        DelinquentLeaderUpdate { id: Pubkey },
    }

    struct ClusterState {
        state_updates_receiver: mpsc::Receiver<ClusterStateUpdate>,
        nodes: HashMap<Pubkey, SocketAddr>,
        nodes_info: Vec<RpcContactInfo>,
        slot_leaders: Vec<String>,
        slot: Slot,
    }

    impl ClusterState {
        fn update_nodes_and_slot_leaders(&mut self) {
            self.nodes_info = self
                .nodes
                .iter()
                .map(|(pubkey, socket)| create_node_info(pubkey.to_string(), *socket))
                .collect();

            self.slot_leaders = generate_slot_leaders(&self.nodes)
                .into_iter()
                .map(|pubkey| pubkey.to_string())
                .collect();
        }

        fn update_state(&mut self) {
            while let Ok(update) = self.state_updates_receiver.try_recv() {
                match update {
                    ClusterStateUpdate::DelinquentLeaderUpdate { id } => {
                        self.slot_leaders.push(id.to_string());
                    }
                    ClusterStateUpdate::IncrementSlot(increment) => {
                        self.slot = self.slot.saturating_add(increment);

                        if !self.slot_leaders.is_empty() {
                            let k = (increment % self.slot_leaders.len() as u64) as usize;
                            self.slot_leaders.rotate_left(k);
                        }
                    }
                    ClusterStateUpdate::ChangeOrAddNode { id, addr } => {
                        self.nodes.insert(id, addr);
                        self.update_nodes_and_slot_leaders();
                    }
                    ClusterStateUpdate::RemoveNode { id } => {
                        self.nodes.remove(&id);
                        self.update_nodes_and_slot_leaders();
                    }
                }
            }
        }
    }

    struct MockSender {
        cluster_state: Arc<RwLock<ClusterState>>,
        slots_in_epoch: u64,

        plans: HashMap<&'static str, Plan>,
        num_calls: AtomicUsize,
    }

    impl MockSender {
        fn new(
            state_updates_receiver: mpsc::Receiver<ClusterStateUpdate>,
            nodes: HashMap<Pubkey, SocketAddr>,
            slot: Slot,
            slots_in_epoch: u64,
            plans: HashMap<&'static str, Plan>,
        ) -> Self {
            let mut cluster_state = ClusterState {
                state_updates_receiver,
                nodes,
                nodes_info: vec![],
                slot_leaders: vec![],
                slot,
            };
            cluster_state.update_nodes_and_slot_leaders();
            Self {
                cluster_state: Arc::new(RwLock::new(cluster_state)),
                slots_in_epoch,
                plans,
                num_calls: AtomicUsize::new(0),
            }
        }

        fn next_step(&self, method: &str) -> Step {
            {
                self.cluster_state.write().unwrap().update_state();
            }
            self.plans.get(method).unwrap().next()
        }

        fn get_slot(&self) -> client_error::Result<serde_json::Value> {
            match self.next_step("getSlot") {
                Step::Ok => {
                    let guard = self.cluster_state.read().unwrap();
                    Ok(json![guard.slot])
                }
                Step::Fail => Err(request::RpcError::RpcRequestError(
                    "getSlot mock failure".to_string(),
                )
                .into()),
                Step::Null => Ok(Value::Null),
            }
        }

        fn get_slot_leaders(&self) -> client_error::Result<serde_json::Value> {
            match self.next_step("getSlotLeaders") {
                Step::Ok => {
                    let guard = self.cluster_state.read().unwrap();
                    Ok(serde_json::to_value(guard.slot_leaders.clone())?)
                }
                Step::Fail => Err(request::RpcError::RpcRequestError(
                    "getSlotLeaders mock failure".to_string(),
                )
                .into()),
                Step::Null => Ok(Value::Null),
            }
        }

        fn get_cluster_nodes(&self) -> client_error::Result<serde_json::Value> {
            match self.next_step("getClusterNodes") {
                Step::Ok => {
                    let guard = self.cluster_state.read().unwrap();
                    Ok(serde_json::to_value(guard.nodes_info.clone())?)
                }
                Step::Fail => Err(request::RpcError::RpcRequestError(
                    "getClusterNodes mock failure".to_string(),
                )
                .into()),
                Step::Null => Ok(Value::Null),
            }
        }

        fn get_epoch_schedule(&self) -> client_error::Result<serde_json::Value> {
            Ok(serde_json::to_value(EpochSchedule::new(
                self.slots_in_epoch,
            ))?)
        }
    }

    fn create_node_info(pubkey: String, socket_addr: SocketAddr) -> RpcContactInfo {
        RpcContactInfo {
            pubkey,
            gossip: None,
            tvu: None,
            tpu: None,
            tpu_quic: Some(socket_addr),
            tpu_forwards: None,
            tpu_forwards_quic: None,
            tpu_vote: None,
            serve_repair: None,
            rpc: None,
            pubsub: None,
            version: Some("1.0.0 c375ce1f".to_string()),
            feature_set: None,
            shred_version: None,
        }
    }

    // leader schedule is as short as number of leaders by 4.
    fn generate_slot_leaders(nodes: &HashMap<Pubkey, SocketAddr>) -> Vec<Pubkey> {
        let mut pubkeys: Vec<Pubkey> = nodes
            .keys()
            .flat_map(|k| std::iter::repeat_n(*k, NUM_CONSECUTIVE_LEADER_SLOTS as usize))
            .collect();
        pubkeys.sort();
        pubkeys
    }

    #[async_trait]
    impl RpcSender for MockSender {
        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats {
                request_count: self.num_calls.load(Ordering::Relaxed),
                ..Default::default()
            }
        }

        async fn send(
            &self,
            request: RpcRequest,
            params: serde_json::Value,
        ) -> client_error::Result<serde_json::Value> {
            self.num_calls.fetch_add(1, Ordering::Relaxed);
            let method = &request.build_request_json(42, params.clone())["method"];

            match method.as_str().unwrap() {
                "getSlot" => self.get_slot(),
                "getSlotLeaders" => self.get_slot_leaders(),
                "getClusterNodes" => self.get_cluster_nodes(),
                "getEpochSchedule" => self.get_epoch_schedule(),

                _ => unimplemented!("MockSender does not support method: {method}"),
            }
        }

        fn url(&self) -> String {
            "MockSender".to_string()
        }
    }

    fn generate_mock_nodes(count: usize, first_port: u16) -> HashMap<Pubkey, SocketAddr> {
        (0..count)
            .map(|i| {
                let pubkey = Pubkey::new_unique();
                let ip = Ipv4Addr::new(10, 0, 0, i as u8 + 1);
                let port = first_port + i as u16;
                (pubkey, SocketAddr::new(IpAddr::V4(ip), port))
            })
            .collect()
    }

    fn create_mock_rpc_client(
        state_update_receiver: mpsc::Receiver<ClusterStateUpdate>,
        nodes: HashMap<Pubkey, SocketAddr>,
        slot: Slot,
        slots_in_epoch: u64,
    ) -> Arc<RpcClient> {
        let mut plans: HashMap<&'static str, Plan> = HashMap::new();
        plans.insert("getClusterNodes", Plan::new(vec![Step::Ok]));
        plans.insert("getSlot", Plan::new(vec![Step::Ok]));
        plans.insert("getSlotLeaders", Plan::new(vec![Step::Ok]));
        plans.insert("getEpochSchedule", Plan::new(vec![Step::Ok]));

        Arc::new(RpcClient::new_sender(
            MockSender::new(state_update_receiver, nodes, slot, slots_in_epoch, plans),
            RpcClientConfig::with_commitment(CommitmentConfig::default()),
        ))
    }

    #[tokio::test]
    async fn test_successfully_refresh_cluster_nodes() {
        const REFRESH_EVERY: Duration = Duration::from_secs(1);
        let mut state = LeaderTpuCacheServiceState::new(0, 0, 0, vec![], vec![]);

        let mut last_refresh = Instant::now() - REFRESH_EVERY;

        let num_leaders = 3;
        let slot = 101;
        let slots_in_epoch = 32;
        let nodes = generate_mock_nodes(num_leaders, 8000);
        let (_state_update_sender, state_update_receiver) = mpsc::channel(1);
        let rpc_client =
            create_mock_rpc_client(state_update_receiver, nodes.clone(), slot, slots_in_epoch);

        let estimated_slot_duration = Duration::from_millis(400);
        let (_tx, rx) = tokio::sync::watch::channel((slot, timestamp(), estimated_slot_duration));

        assert!(state
            .update(&mut last_refresh, REFRESH_EVERY, &rpc_client, &rx)
            .await
            .is_ok());
        assert_eq!(
            state,
            LeaderTpuCacheServiceState {
                first_slot: slot,
                leaders: generate_slot_leaders(&nodes),
                leader_tpu_map: nodes.clone(),
                slots_in_epoch,
                last_slot_in_epoch: (slot / slots_in_epoch + 1) * slots_in_epoch - 1,
            }
        );

        let mut sorted_nodes: Vec<Pubkey> = nodes.keys().cloned().collect();
        sorted_nodes.sort();

        for (i, expected_leader) in sorted_nodes
            .iter()
            .flat_map(|k| std::iter::repeat_n(k, NUM_CONSECUTIVE_LEADER_SLOTS as usize))
            .enumerate()
        {
            assert_eq!(
                state.get_slot_leader(slot + i as u64),
                Some(expected_leader),
                "Leader for slot {} should be {}",
                slot + i as u64,
                expected_leader
            );
        }

        let expected_sockets: Vec<SocketAddr> =
            vec![nodes[&sorted_nodes[0]], nodes[&sorted_nodes[1]]];
        assert_eq!(
            state.get_leader_sockets(slot, 2 * NUM_CONSECUTIVE_LEADER_SLOTS),
            expected_sockets
        );
    }

    fn create_failing_mock_rpc_client(
        slot: Slot,
        slots_in_epoch: u64,
        steps: Vec<Step>,
    ) -> Arc<RpcClient> {
        let nodes = generate_mock_nodes(1, 8000);
        let mut plans: HashMap<&'static str, Plan> = HashMap::new();

        plans.insert("getClusterNodes", Plan::new(steps.clone()));
        plans.insert("getSlot", Plan::new(steps.clone()));
        plans.insert("getSlotLeaders", Plan::new(steps.clone()));
        plans.insert("getEpochSchedule", Plan::new(steps.clone()));

        let (_state_update_sender, state_update_receiver) = mpsc::channel(1);
        Arc::new(RpcClient::new_sender(
            MockSender::new(state_update_receiver, nodes, slot, slots_in_epoch, plans),
            RpcClientConfig::with_commitment(CommitmentConfig::default()),
        ))
    }

    #[tokio::test]
    async fn test_update_with_failing_cluster_nodes_request() {
        const REFRESH_EVERY: Duration = Duration::from_secs(1);
        let mut state = LeaderTpuCacheServiceState::new(0, 0, 0, vec![], vec![]);

        let mut last_refresh = Instant::now() - REFRESH_EVERY;

        let slot = 0;
        let rpc_client = create_failing_mock_rpc_client(slot, 32, vec![Step::Fail]);

        let estimated_slot_duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let (_tx, rx) = tokio::sync::watch::channel((slot, timestamp(), estimated_slot_duration));

        assert!(state
            .update(&mut last_refresh, REFRESH_EVERY, &rpc_client, &rx)
            .await
            .is_err());
        assert_eq!(
            state,
            LeaderTpuCacheServiceState {
                first_slot: slot,
                leaders: vec![],
                leader_tpu_map: HashMap::new(),
                slots_in_epoch: 0,
                last_slot_in_epoch: 0,
            }
        );
    }

    #[tokio::test]
    async fn test_run_fails_when_rpc_client_fails_after_max_retries() {
        let slot = 0;

        let rpc_client = create_failing_mock_rpc_client(slot, 32, vec![Step::Fail]);

        let (slot_sender, slot_receiver) = tokio::sync::watch::channel((
            0u64,
            timestamp(),
            Duration::from_millis(DEFAULT_MS_PER_SLOT),
        ));
        let (leaders_sender, _leaders_receiver) =
            tokio::sync::watch::channel((0u64, Vec::<SocketAddr>::new()));

        let cancel = CancellationToken::new();

        let leader_cache_service = LeaderTpuCacheService::new(
            rpc_client.clone(),
            slot_receiver,
            leaders_sender,
            LeaderTpuCacheServiceConfig {
                lookahead_leaders: 1,
                refresh_every: Duration::from_secs(1),
                max_consecutive_failures: 1,
            },
            cancel.clone(),
        )
        .await
        .unwrap();
        let service_handle = tokio::spawn(leader_cache_service.run());

        slot_sender
            .send((1, timestamp(), Duration::from_millis(DEFAULT_MS_PER_SLOT)))
            .unwrap();

        let result = timeout(Duration::from_secs(1), service_handle).await;
        assert!(result.unwrap().unwrap().is_err());
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_stops_on_cancel() {
        let nodes = generate_mock_nodes(2, 8000);
        let (_state_update_sender, state_update_receiver) = mpsc::channel(1);
        let rpc_client = create_mock_rpc_client(state_update_receiver, nodes, 0, 32);

        let (_slot_sender, slot_receiver) = tokio::sync::watch::channel((
            0u64,
            timestamp(),
            Duration::from_millis(DEFAULT_MS_PER_SLOT),
        ));
        let (leaders_sender, _leaders_receiver) =
            tokio::sync::watch::channel((0u64, Vec::<SocketAddr>::new()));

        let cancel = CancellationToken::new();
        let leader_cache_service = LeaderTpuCacheService::new(
            rpc_client.clone(),
            slot_receiver,
            leaders_sender,
            LeaderTpuCacheServiceConfig {
                lookahead_leaders: 1,
                refresh_every: Duration::from_secs(1),
                max_consecutive_failures: 1,
            },
            cancel.clone(),
        )
        .await
        .unwrap();
        let service_handle = tokio::spawn(leader_cache_service.run());

        cancel.cancel();
        service_handle.await.unwrap().unwrap();
    }

    fn get_expected_sockets(
        nodes: &HashMap<Pubkey, SocketAddr>,
        start_index: usize,
        lookahead_leaders: u64,
    ) -> Vec<SocketAddr> {
        let lookahead_slots = lookahead_leaders * NUM_CONSECUTIVE_LEADER_SLOTS;
        let expected_leaders = generate_slot_leaders(nodes);
        debug!("Expected leaders: {:?}", expected_leaders);

        let expected_keys_slice: Vec<_> = expected_leaders
            .iter()
            .cycle()
            .skip(start_index % expected_leaders.len())
            .take(lookahead_slots as usize)
            .step_by(NUM_CONSECUTIVE_LEADER_SLOTS as usize)
            .cloned()
            .collect();

        expected_keys_slice
            .iter()
            .filter_map(|pk| nodes.get(pk).copied())
            .collect()
    }

    struct TestFixture {
        rpc_client: Arc<RpcClient>,
        nodes: HashMap<Pubkey, SocketAddr>,
        slot_sender: watch::Sender<(u64, u64, Duration)>,
        state_update_sender: mpsc::Sender<ClusterStateUpdate>,
        leaders_receiver: watch::Receiver<(u64, Vec<SocketAddr>)>,
        service_handle: JoinHandle<Result<(), NodeAddressServiceError>>,
        estimated_slot_duration: Duration,
        cancel: CancellationToken,
    }

    impl TestFixture {
        async fn setup(
            slots_in_epoch: u64,
            current_slot: u64,
            num_nodes: usize,
            lookahead_leaders: u64,
            refresh_every: Duration,
            max_consecutive_failures: usize,
        ) -> Self {
            let mut plans: HashMap<&'static str, Plan> = HashMap::new();
            plans.insert("getClusterNodes", Plan::new(vec![Step::Ok]));
            plans.insert("getSlot", Plan::new(vec![Step::Ok]));
            plans.insert("getSlotLeaders", Plan::new(vec![Step::Ok]));
            plans.insert("getEpochSchedule", Plan::new(vec![Step::Ok]));

            Self::setup_with_plan(
                slots_in_epoch,
                current_slot,
                num_nodes,
                lookahead_leaders,
                refresh_every,
                max_consecutive_failures,
                plans,
            )
            .await
        }

        async fn setup_with_plan(
            slots_in_epoch: u64,
            current_slot: u64,
            num_nodes: usize,
            lookahead_leaders: u64,
            refresh_every: Duration,
            max_consecutive_failures: usize,
            plans: HashMap<&'static str, Plan>,
        ) -> Self {
            let nodes = generate_mock_nodes(num_nodes, 8000);
            let (state_update_sender, state_update_receiver) = mpsc::channel(12);

            let rpc_client = Arc::new(RpcClient::new_sender(
                MockSender::new(
                    state_update_receiver,
                    nodes.clone(),
                    current_slot,
                    slots_in_epoch,
                    plans,
                ),
                RpcClientConfig::with_commitment(CommitmentConfig::default()),
            ));

            let estimated_slot_duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
            let (slot_sender, slot_receiver) =
                watch::channel((current_slot, timestamp(), estimated_slot_duration));
            let (leaders_sender, leaders_receiver) =
                watch::channel((0u64, Vec::<SocketAddr>::new()));

            // run service
            let cancel = CancellationToken::new();
            let leader_cache_service = LeaderTpuCacheService::new(
                rpc_client.clone(),
                slot_receiver,
                leaders_sender,
                LeaderTpuCacheServiceConfig {
                    lookahead_leaders,
                    refresh_every,
                    max_consecutive_failures,
                },
                cancel.clone(),
            )
            .await
            .unwrap();
            let service_handle = tokio::spawn(leader_cache_service.run());
            Self {
                rpc_client,
                nodes,
                slot_sender,
                state_update_sender,
                leaders_receiver,
                service_handle,
                estimated_slot_duration,
                cancel,
            }
        }

        async fn teardown(self) -> Result<(), NodeAddressServiceError> {
            self.cancel.cancel();
            self.service_handle.await.unwrap()
        }
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_emits_leaders_on_slot_update() {
        let slots_in_epoch = 32;
        let current_slot: u64 = 101;
        let num_nodes = 3;
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(2 * DEFAULT_MS_PER_SLOT);
        let mut fixture = TestFixture::setup(
            slots_in_epoch,
            current_slot,
            num_nodes,
            lookahead_leaders,
            refresh_every,
            1,
        )
        .await;

        // trigger a new slot update
        let next_slot = current_slot + 1;
        fixture
            .slot_sender
            .send((next_slot, timestamp(), fixture.estimated_slot_duration))
            .unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.borrow().clone();
        assert_eq!(emitted_slot, next_slot);

        let expected_sockets = get_expected_sockets(&fixture.nodes, 0, lookahead_leaders);
        assert_eq!(emitted_sockets, expected_sockets);

        fixture.teardown().await.unwrap();
    }

    async fn update_ports(
        state_update_sender: &mpsc::Sender<ClusterStateUpdate>,
        nodes: &mut HashMap<Pubkey, SocketAddr>,
        first_port: u16,
    ) {
        let identities: Vec<Pubkey> = nodes.keys().cloned().collect();

        for (i, id) in identities.iter().enumerate() {
            if let Some(addr) = nodes.get_mut(id) {
                addr.set_port(first_port + i as u16);
            }
            state_update_sender
                .send(ClusterStateUpdate::ChangeOrAddNode {
                    id: *id,
                    addr: nodes[id],
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_updates_leaders() {
        let slots_in_epoch = 32;
        let current_slot: u64 = 101;
        let num_nodes = 3;
        let estimated_slot_duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(0);

        let mut fixture = TestFixture::setup(
            slots_in_epoch,
            current_slot,
            num_nodes,
            lookahead_leaders,
            refresh_every,
            1,
        )
        .await;

        update_ports(&fixture.state_update_sender, &mut fixture.nodes, 1000).await;

        // trigger a new slot update
        let next_slot = current_slot + 1;
        fixture
            .slot_sender
            .send((next_slot, timestamp(), estimated_slot_duration))
            .unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.borrow().clone();
        assert_eq!(emitted_slot, next_slot);

        let expected_sockets = get_expected_sockets(&fixture.nodes, 0, lookahead_leaders);
        assert_eq!(
            emitted_sockets, expected_sockets,
            "nodes {:?}",
            fixture.nodes
        );

        fixture.teardown().await.unwrap();
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_delinquent_node() {
        let slots_in_epoch = 32;
        let current_slot: u64 = 101;
        let num_nodes = 1;
        let estimated_slot_duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(2 * DEFAULT_MS_PER_SLOT);

        let mut fixture = TestFixture::setup(
            slots_in_epoch,
            current_slot,
            num_nodes,
            lookahead_leaders,
            refresh_every,
            1,
        )
        .await;

        // Add delinquent node
        let delinquent_node = Pubkey::new_unique();
        fixture
            .state_update_sender
            .send(ClusterStateUpdate::DelinquentLeaderUpdate {
                id: delinquent_node,
            })
            .await
            .unwrap();

        // trigger a new slot update
        let next_slot = current_slot + 1;
        fixture
            .slot_sender
            .send((next_slot, timestamp(), estimated_slot_duration))
            .unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.borrow().clone();
        assert_eq!(emitted_slot, next_slot);

        let expected_sockets = get_expected_sockets(&fixture.nodes, 0, 1);
        assert_eq!(emitted_sockets, expected_sockets);

        fixture.teardown().await.unwrap();
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_epoch_change() {
        let slots_in_epoch = 32;
        let current_slot: u64 = 101;
        let num_nodes = 1;
        let estimated_slot_duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(0);
        let mut fixture = TestFixture::setup(
            slots_in_epoch,
            current_slot,
            num_nodes,
            lookahead_leaders,
            refresh_every,
            1,
        )
        .await;

        let node = *fixture.nodes.keys().next().unwrap();
        fixture
            .state_update_sender
            .send(ClusterStateUpdate::RemoveNode { id: node })
            .await
            .unwrap();

        let new_node = Pubkey::new_unique();
        let new_socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1000);
        fixture
            .state_update_sender
            .send(ClusterStateUpdate::ChangeOrAddNode {
                id: new_node,
                addr: new_socket,
            })
            .await
            .unwrap();

        fixture
            .state_update_sender
            .send(ClusterStateUpdate::IncrementSlot(
                slots_in_epoch, // increment to next epoch
            ))
            .await
            .unwrap();

        let next_slot = current_slot + slots_in_epoch;
        // trigger a new slot update
        fixture
            .slot_sender
            .send((next_slot, timestamp(), estimated_slot_duration))
            .unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.borrow().clone();
        assert_eq!(emitted_slot, next_slot);

        let expected_sockets = vec![new_socket];
        assert_eq!(emitted_sockets, expected_sockets);

        fixture.teardown().await.unwrap();
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_resilient_to_rpc_failures() {
        let slots_in_epoch = 32;
        let current_slot: u64 = 101;
        let num_nodes = 3;
        let estimated_slot_duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(0);

        let sequence = vec![Step::Ok, Step::Fail, Step::Ok, Step::Null, Step::Ok];
        let mut plans: HashMap<&'static str, Plan> = HashMap::new();
        plans.insert("getClusterNodes", Plan::new(sequence.clone()));
        plans.insert("getSlot", Plan::new(sequence.clone()));
        plans.insert("getSlotLeaders", Plan::new(sequence.clone()));
        plans.insert("getEpochSchedule", Plan::new(sequence.clone()));
        let mut fixture = TestFixture::setup_with_plan(
            slots_in_epoch,
            current_slot,
            num_nodes,
            lookahead_leaders,
            refresh_every,
            2,
            plans,
        )
        .await;

        let mut next_slot = current_slot;
        update_ports(&fixture.state_update_sender, &mut fixture.nodes, 1000).await;
        for _ in 0..(2 * NUM_CONSECUTIVE_LEADER_SLOTS + 1) {
            fixture
                .state_update_sender
                .send(ClusterStateUpdate::IncrementSlot(1))
                .await
                .unwrap();

            // trigger a new slot update
            next_slot = next_slot.saturating_add(1);
            fixture
                .slot_sender
                .send((next_slot, timestamp(), estimated_slot_duration))
                .unwrap();
            sleep(Duration::from_millis(DEFAULT_MS_PER_SLOT)).await;
        }

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );
        assert_eq!(fixture.rpc_client.get_transport_stats().request_count, 19);

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.borrow().clone();
        debug!("emitted_slot: {emitted_slot}, emitted_sockets: {emitted_sockets:?}");
        assert_eq!(emitted_slot, next_slot);

        let expected_sockets = get_expected_sockets(
            &fixture.nodes,
            2 * NUM_CONSECUTIVE_LEADER_SLOTS as usize,
            lookahead_leaders,
        );
        assert_eq!(
            emitted_sockets, expected_sockets,
            "nodes {:?}",
            fixture.nodes
        );

        fixture.teardown().await.unwrap();
    }
}
