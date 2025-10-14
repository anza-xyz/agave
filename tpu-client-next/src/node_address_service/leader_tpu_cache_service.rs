#![allow(clippy::arithmetic_side_effects)]
use {
    crate::node_address_service::{NodeAddressServiceError, SlotReceiver},
    async_trait::async_trait,
    log::*,
    solana_clock::{Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_pubkey::Pubkey,
    solana_quic_definitions::QUIC_PORT_OFFSET,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::response::RpcContactInfo,
    std::{collections::HashMap, future::Future, net::SocketAddr, str::FromStr, sync::Arc},
    tokio::{
        sync::watch,
        task::JoinHandle,
        time::{interval, sleep, Duration, Instant},
    },
    tokio_util::sync::CancellationToken,
};

/// Maximum number of slots used to build TPU socket fanout set
const MAX_FANOUT_SLOTS: u64 = 100;

/// Configuration for the [`LeaderTpuCacheService`].
#[derive(Debug, Clone)]
pub struct LeaderTpuCacheServiceConfig {
    /// max number of leaders to look ahead for
    pub lookahead_leaders: usize,
    /// how often to refresh cluster nodes info
    pub refresh_nodes_info_every: Duration,
    /// maximum number of consecutive failures to tolerate before
    pub max_consecutive_failures: usize,
}

/// [`LeaderTpuCacheService`] is a background task that tracks the current and
/// upcoming Solana leader nodes and updates their TPU socket addresses in a
/// watch channel for downstream consumers.
pub struct LeaderTpuCacheService {
    handle: JoinHandle<Result<(), NodeAddressServiceError>>,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct LeaderUpdateReceiver {
    receiver: watch::Receiver<(Slot, Vec<SocketAddr>)>,
}

impl LeaderUpdateReceiver {
    pub fn leaders(&self) -> Vec<SocketAddr> {
        let (_, leaders) = self.receiver.borrow().clone();
        leaders
    }
}

impl LeaderTpuCacheService {
    pub async fn run(
        rpc_client: Arc<impl ClusterInfoProvider + 'static>,
        mut slot_receiver: SlotReceiver,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<(LeaderUpdateReceiver, Self), NodeAddressServiceError> {
        let lookahead_slots =
            (config.lookahead_leaders as u64).saturating_mul(NUM_CONSECUTIVE_LEADER_SLOTS);

        let (mut leader_tpu_map, mut epoch_info, mut slot_leaders) = Self::initialize_state(
            rpc_client.as_ref(),
            slot_receiver.clone(),
            config.max_consecutive_failures,
        )
        .await?;
        let (current_slot, _timestamp) = slot_receiver.slot_with_timestamp();
        let leaders = Self::leader_sockets(
            current_slot,
            lookahead_slots,
            &slot_leaders,
            &leader_tpu_map,
        );

        let (leaders_sender, leaders_receiver) = watch::channel((current_slot, leaders));

        let cancel_clone = cancel.clone();
        let main_loop = async move {
            let mut num_consequent_failures: usize = 0;
            let mut refresh_tpu_interval = interval(config.refresh_nodes_info_every);
            loop {
                tokio::select! {
                    _ = refresh_tpu_interval.tick() => {
                        try_update(
                            "cluster TPU ports",
                            &mut leader_tpu_map,
                            || LeaderTpuMap::new(rpc_client.as_ref()),
                            &mut num_consequent_failures,
                            config.max_consecutive_failures,
                        ).await?;
                        debug!("Updated cluster TPU ports");
                    }
                    res = slot_receiver.changed() => {
                        debug!("Changed slot receiver");
                        if let Err(e) = res {
                            warn!("Slot receiver channel closed: {e}");
                            break;
                        }

                        let (estimated_current_slot, _start_time) = slot_receiver.slot_with_timestamp();
                        if estimated_current_slot > epoch_info.last_slot_in_epoch {
                            try_update(
                                "epoch info",
                                &mut epoch_info,
                                || EpochInfo::new(rpc_client.as_ref(), estimated_current_slot),
                                &mut num_consequent_failures,
                                config.max_consecutive_failures,
                            ).await?;
                        }
                        if estimated_current_slot > slot_leaders.last_slot().saturating_sub(MAX_FANOUT_SLOTS) {
                            try_update(
                                "slot leaders",
                                &mut slot_leaders,
                                || SlotLeaders::new(rpc_client.as_ref(), estimated_current_slot, epoch_info.slots_in_epoch),
                                &mut num_consequent_failures,
                                config.max_consecutive_failures,
                            ).await?;
                        }

                        let (current_slot, _timestamp) = slot_receiver.slot_with_timestamp();
                        let leaders = Self::leader_sockets(current_slot, lookahead_slots, &slot_leaders, &leader_tpu_map);

                        if let Err(e) = leaders_sender.send((current_slot, leaders)) {
                            warn!("Unexpectedly dropped leaders_sender: {e}");
                            return Err(NodeAddressServiceError::ChannelClosed);
                        }
                    }

                    _ = cancel.cancelled() => {
                        info!("Cancel signal received, stopping LeaderTpuCacheService.");
                        break;
                    }
                }
            }
            Ok(())
        };

        let handle = tokio::spawn(main_loop);

        Ok((
            LeaderUpdateReceiver {
                receiver: leaders_receiver,
            },
            Self {
                handle,
                cancel: cancel_clone,
            },
        ))
    }

    pub async fn shutdown(self) -> Result<(), NodeAddressServiceError> {
        self.cancel.cancel();
        self.handle.await??;
        Ok(())
    }

    /// Get the TPU sockets for the current and upcoming leaders according to
    /// fanout size.
    fn leader_sockets(
        estimated_current_slot: Slot,
        fanout_slots: u64,
        slot_leaders: &SlotLeaders,
        leader_tpu_map: &LeaderTpuMap,
    ) -> Vec<SocketAddr> {
        let mut leader_sockets =
            Vec::with_capacity(fanout_slots.div_ceil(NUM_CONSECUTIVE_LEADER_SLOTS) as usize);
        // `first_slot` might have been advanced since caller last read the
        // `estimated_current_slot` value. Take the greater of the two values to
        // ensure we are reading from the latest leader schedule.
        let current_slot = std::cmp::max(estimated_current_slot, slot_leaders.first_slot);
        for leader_slot in (current_slot..current_slot + fanout_slots)
            .step_by(NUM_CONSECUTIVE_LEADER_SLOTS as usize)
        {
            if let Some(leader) = slot_leaders.slot_leader(leader_slot) {
                if let Some(tpu_socket) = leader_tpu_map.get(leader) {
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
                    slot_leaders.first_slot,
                    slot_leaders.last_slot()
                );
            }
        }

        leader_sockets
    }

    async fn initialize_state(
        rpc_client: &impl ClusterInfoProvider,
        slot_receiver: SlotReceiver,
        max_attempts: usize,
    ) -> Result<(LeaderTpuMap, EpochInfo, SlotLeaders), NodeAddressServiceError> {
        const ATTEMPTS_SLEEP_DURATION: Duration = Duration::from_millis(1000);
        let mut leader_tpu_map = None;
        let mut epoch_info = None;
        let mut slot_leaders = None;
        let mut num_attempts = 0;
        while num_attempts < max_attempts {
            if leader_tpu_map.is_none() {
                leader_tpu_map = LeaderTpuMap::new(rpc_client).await.ok();
            }
            if epoch_info.is_none() {
                epoch_info = EpochInfo::new(rpc_client, slot_receiver.slot_with_timestamp().0)
                    .await
                    .ok();
            }

            if let Some(epoch_info) = &epoch_info {
                if slot_leaders.is_none() {
                    slot_leaders = SlotLeaders::new(
                        rpc_client,
                        slot_receiver.slot_with_timestamp().0,
                        epoch_info.slots_in_epoch,
                    )
                    .await
                    .ok();
                }
            }
            if leader_tpu_map.is_some() && epoch_info.is_some() && slot_leaders.is_some() {
                break;
            }
            num_attempts = num_attempts.saturating_add(1);
            sleep(ATTEMPTS_SLEEP_DURATION).await;
        }
        if num_attempts >= max_attempts {
            Err(NodeAddressServiceError::InitializationFailed)
        } else {
            Ok((
                leader_tpu_map.unwrap(),
                epoch_info.unwrap(),
                slot_leaders.unwrap(),
            ))
        }
    }
}

async fn try_update<F, Fut, T>(
    label: &str,
    data: &mut T,
    make_call: F,
    num_failures: &mut usize,
    max_failures: usize,
) -> Result<(), NodeAddressServiceError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, NodeAddressServiceError>>,
{
    match make_call().await {
        Ok(result) => {
            *num_failures = 0;
            debug!("{label} updated successfully");
            *data = result;
            Ok(())
        }
        Err(e) => {
            *num_failures = num_failures.saturating_add(1);
            warn!("Failed to update {label}: {e} ({num_failures} consecutive failures)",);

            if *num_failures >= max_failures {
                error!("Max consecutive failures for {label}, giving up.");
                Err(e)
            } else {
                Ok(())
            }
        }
    }
}

#[derive(PartialEq, Debug)]
struct LeaderTpuMap {
    last_cluster_refresh: Instant,
    leader_tpu_map: HashMap<Pubkey, SocketAddr>,
}

impl LeaderTpuMap {
    async fn new(rpc_client: &impl ClusterInfoProvider) -> Result<Self, NodeAddressServiceError> {
        let leader_tpu_map = rpc_client.leader_tpu_map().await?;
        Ok(Self {
            last_cluster_refresh: Instant::now(),
            leader_tpu_map,
        })
    }

    fn get(&self, leader: &Pubkey) -> Option<&SocketAddr> {
        self.leader_tpu_map.get(leader)
    }
}

#[derive(PartialEq, Debug)]
struct SlotLeaders {
    first_slot: Slot,
    leaders: Vec<Pubkey>,
}

impl SlotLeaders {
    async fn new(
        rpc_client: &impl ClusterInfoProvider,
        estimated_current_slot: Slot,
        slots_in_epoch: Slot,
    ) -> Result<Self, NodeAddressServiceError> {
        Ok(Self {
            first_slot: estimated_current_slot,
            leaders: rpc_client
                .slot_leaders(estimated_current_slot, slots_in_epoch)
                .await?,
        })
    }

    fn last_slot(&self) -> Slot {
        self.first_slot + self.leaders.len().saturating_sub(1) as u64
    }

    fn slot_leader(&self, slot: Slot) -> Option<&Pubkey> {
        if slot >= self.first_slot {
            let index = slot - self.first_slot;
            self.leaders.get(index as usize)
        } else {
            None
        }
    }
}

#[derive(PartialEq, Debug)]
struct EpochInfo {
    slots_in_epoch: Slot,
    last_slot_in_epoch: Slot,
}

impl EpochInfo {
    async fn new(
        rpc_client: &impl ClusterInfoProvider,
        estimated_current_slot: Slot,
    ) -> Result<Self, NodeAddressServiceError> {
        let (slots_in_epoch, last_slot_in_epoch) =
            rpc_client.epoch_info(estimated_current_slot).await?;
        Ok(Self {
            slots_in_epoch,
            last_slot_in_epoch,
        })
    }
}

#[async_trait]
pub trait ClusterInfoProvider: Send + Sync {
    async fn leader_tpu_map(&self) -> Result<HashMap<Pubkey, SocketAddr>, NodeAddressServiceError>;
    async fn epoch_info(
        &self,
        estimated_current_slot: Slot,
    ) -> Result<(Slot, Slot), NodeAddressServiceError>;
    async fn slot_leaders(
        &self,
        estimated_current_slot: Slot,
        slots_in_epoch: Slot,
    ) -> Result<Vec<Pubkey>, NodeAddressServiceError>;
}

#[async_trait]
impl ClusterInfoProvider for RpcClient {
    async fn leader_tpu_map(&self) -> Result<HashMap<Pubkey, SocketAddr>, NodeAddressServiceError> {
        let cluster_nodes = self.get_cluster_nodes().await;
        match cluster_nodes {
            Ok(cluster_nodes) => Ok(extract_cluster_tpu_sockets(cluster_nodes)),
            Err(err) => Err(NodeAddressServiceError::RpcError(err)),
        }
    }

    async fn epoch_info(
        &self,
        estimated_current_slot: Slot,
    ) -> Result<(Slot, Slot), NodeAddressServiceError> {
        match self.get_epoch_schedule().await {
            Ok(epoch_schedule) => {
                let epoch = epoch_schedule.get_epoch(estimated_current_slot);
                let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
                let last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);
                debug!(
                    "Updated slots in epoch: {slots_in_epoch}, last slot in epoch: {last_slot_in_epoch}",
                );
                Ok((slots_in_epoch, last_slot_in_epoch))
            }
            Err(err) => Err(NodeAddressServiceError::RpcError(err)),
        }
    }

    async fn slot_leaders(
        &self,
        estimated_current_slot: Slot,
        slots_in_epoch: Slot,
    ) -> Result<Vec<Pubkey>, NodeAddressServiceError> {
        let slot_leaders = self
            .get_slot_leaders(estimated_current_slot, fanout(slots_in_epoch))
            .await;
        debug!("Fetched slot leaders from slot {estimated_current_slot} for {slots_in_epoch}. ");
        match slot_leaders {
            Ok(slot_leaders) => Ok(slot_leaders),
            Err(err) => Err(NodeAddressServiceError::RpcError(err)),
        }
    }
}

fn fanout(slots_in_epoch: Slot) -> Slot {
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
        solana_time_utils::timestamp,
        std::{
            net::{IpAddr, Ipv4Addr},
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc, RwLock,
            },
        },
        tokio::{
            sync::mpsc,
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
                    let guard: std::sync::RwLockReadGuard<'_, ClusterState> =
                        self.cluster_state.read().unwrap();
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
        pubkeys.repeat(4)
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
        let num_leaders = 3;
        let slot = 101;
        let slots_in_epoch = 32;
        let nodes = generate_mock_nodes(num_leaders, 8000);
        let (_state_update_sender, state_update_receiver) = mpsc::channel(1);
        let rpc_client =
            create_mock_rpc_client(state_update_receiver, nodes.clone(), slot, slots_in_epoch);

        let (_tx, receiver) = watch::channel((slot, timestamp()));
        let slot_receiver = SlotReceiver::new(receiver);

        let (leader_tpu_map, epoch_info, slot_leaders) =
            LeaderTpuCacheService::initialize_state(rpc_client.as_ref(), slot_receiver, 5)
                .await
                .unwrap();
        assert_eq!(leader_tpu_map.leader_tpu_map, nodes);
        assert_eq!(epoch_info.slots_in_epoch, slots_in_epoch);
        assert_eq!(
            epoch_info.last_slot_in_epoch,
            (slot / slots_in_epoch + 1) * slots_in_epoch - 1
        );
        assert_eq!(slot_leaders.first_slot, slot);
        assert_eq!(slot_leaders.leaders, generate_slot_leaders(&nodes));

        let mut sorted_nodes: Vec<Pubkey> = nodes.keys().cloned().collect();
        sorted_nodes.sort();

        for (i, expected_leader) in sorted_nodes
            .iter()
            .flat_map(|k| std::iter::repeat_n(k, NUM_CONSECUTIVE_LEADER_SLOTS as usize))
            .enumerate()
        {
            assert_eq!(
                slot_leaders.slot_leader(slot + i as u64),
                Some(expected_leader),
                "Leader for slot {} should be {}",
                slot + i as u64,
                expected_leader
            );
        }

        let expected_sockets: Vec<SocketAddr> =
            vec![nodes[&sorted_nodes[0]], nodes[&sorted_nodes[1]]];
        assert_eq!(
            LeaderTpuCacheService::leader_sockets(
                slot,
                2 * NUM_CONSECUTIVE_LEADER_SLOTS,
                &slot_leaders,
                &leader_tpu_map
            ),
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
        let slot = 0;
        let rpc_client = create_failing_mock_rpc_client(slot, 32, vec![Step::Fail]);

        let (_tx, receiver) = watch::channel((slot, timestamp()));
        let slot_receiver = SlotReceiver::new(receiver);
        assert!(
            LeaderTpuCacheService::initialize_state(rpc_client.as_ref(), slot_receiver, 1)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_run_fails_when_rpc_client_fails_after_max_retries() {
        let slot = 0;

        let rpc_client = create_failing_mock_rpc_client(slot, 32, vec![Step::Fail]);

        let (_slot_sender, slot_receiver) = watch::channel((0u64, timestamp()));
        let slot_receiver = SlotReceiver::new(slot_receiver);

        let cancel = CancellationToken::new();

        let result = LeaderTpuCacheService::run(
            rpc_client.clone(),
            slot_receiver,
            LeaderTpuCacheServiceConfig {
                lookahead_leaders: 1,
                refresh_nodes_info_every: Duration::from_secs(1),
                max_consecutive_failures: 1,
            },
            cancel.clone(),
        )
        .await;

        assert!(result.is_err(), "Initialization should fail.");
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_stops_on_shutdown() {
        let nodes = generate_mock_nodes(2, 8000);
        let (_state_update_sender, state_update_receiver) = mpsc::channel(1);
        let rpc_client = create_mock_rpc_client(state_update_receiver, nodes, 0, 32);

        let (_slot_sender, slot_receiver) = watch::channel((0u64, timestamp()));
        let slot_receiver = SlotReceiver::new(slot_receiver);

        let cancel = CancellationToken::new();
        let (_leader_update_receiver, leader_cache_service) = LeaderTpuCacheService::run(
            rpc_client.clone(),
            slot_receiver,
            LeaderTpuCacheServiceConfig {
                lookahead_leaders: 1,
                refresh_nodes_info_every: Duration::from_secs(1),
                max_consecutive_failures: 1,
            },
            cancel.clone(),
        )
        .await
        .unwrap();

        let result = timeout(Duration::from_secs(1), leader_cache_service.shutdown()).await;
        assert!(result.unwrap().is_ok());
    }

    fn get_expected_sockets(
        nodes: &HashMap<Pubkey, SocketAddr>,
        start_index: usize,
        lookahead_leaders: usize,
    ) -> Vec<SocketAddr> {
        let lookahead_slots = lookahead_leaders * (NUM_CONSECUTIVE_LEADER_SLOTS as usize);
        let expected_leaders = generate_slot_leaders(nodes);
        debug!("Expected leaders: {:?}", expected_leaders);

        let expected_keys_slice: Vec<_> = expected_leaders
            .iter()
            .cycle()
            .skip(start_index % expected_leaders.len())
            .take(lookahead_slots)
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
        slot_sender: watch::Sender<(u64, u64)>,
        state_update_sender: mpsc::Sender<ClusterStateUpdate>,
        leaders_receiver: LeaderUpdateReceiver,
        leader_cache_service: LeaderTpuCacheService,
    }

    impl TestFixture {
        async fn setup(
            slots_in_epoch: u64,
            current_slot: u64,
            num_nodes: usize,
            lookahead_leaders: usize,
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
            lookahead_leaders: usize,
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

            let (slot_sender, slot_receiver) = watch::channel((current_slot, timestamp()));
            let slot_receiver = SlotReceiver::new(slot_receiver);

            // run service
            let cancel = CancellationToken::new();
            let (leaders_receiver, leader_cache_service) = LeaderTpuCacheService::run(
                rpc_client.clone(),
                slot_receiver,
                LeaderTpuCacheServiceConfig {
                    lookahead_leaders,
                    refresh_nodes_info_every: refresh_every,
                    max_consecutive_failures,
                },
                cancel,
            )
            .await
            .unwrap();
            Self {
                rpc_client,
                nodes,
                slot_sender,
                state_update_sender,
                leaders_receiver,
                leader_cache_service,
            }
        }

        async fn teardown(self) -> Result<(), NodeAddressServiceError> {
            self.leader_cache_service.shutdown().await?;
            Ok(())
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
        fixture.slot_sender.send((next_slot, timestamp())).unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.receiver.borrow().clone();
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
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(100);

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
        // to update port information, wait for x2 the refresh duration
        sleep(refresh_every * 2).await;

        // trigger a new slot update
        let next_slot = current_slot + 1;
        fixture.slot_sender.send((next_slot, timestamp())).unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.receiver.borrow().clone();
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
        fixture.slot_sender.send((next_slot, timestamp())).unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.receiver.borrow().clone();
        assert_eq!(emitted_slot, next_slot);

        let expected_sockets = get_expected_sockets(&fixture.nodes, 0, lookahead_leaders);
        assert_eq!(emitted_sockets, expected_sockets);

        fixture.teardown().await.unwrap();
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_epoch_change() {
        let slots_in_epoch = 32;
        let current_slot: u64 = 101;
        let num_nodes = 1;
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(10);
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
        debug!("New node: {new_node}");
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
        // Information about addresses is updated every `refresh_every`
        // duration. So wait for x2 to be sure that it has happen.
        sleep(refresh_every * 2).await;

        let next_slot = current_slot + slots_in_epoch;
        // trigger a new slot update
        fixture.slot_sender.send((next_slot, timestamp())).unwrap();

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.receiver.borrow().clone();
        assert_eq!(emitted_slot, next_slot);

        let expected_sockets = vec![new_socket; lookahead_leaders];
        assert_eq!(emitted_sockets, expected_sockets);

        fixture.teardown().await.unwrap();
    }

    #[tokio::test]
    async fn test_leader_tpu_cache_service_resilient_to_rpc_failures() {
        let slots_in_epoch = 32;
        let current_slot: u64 = 101;
        let num_nodes = 3;
        let lookahead_leaders = 2;
        let refresh_every = Duration::from_millis(100);

        let mut sequence = vec![Step::Ok, Step::Fail, Step::Ok, Step::Null, Step::Ok];
        let mut plans: HashMap<&'static str, Plan> = HashMap::new();
        plans.insert("getClusterNodes", Plan::new(sequence.clone()));
        sequence.rotate_left(1);
        plans.insert("getSlot", Plan::new(sequence.clone()));
        sequence.rotate_left(1);
        plans.insert("getSlotLeaders", Plan::new(sequence.clone()));
        sequence.rotate_left(1);
        plans.insert("getEpochSchedule", Plan::new(sequence.clone()));
        let mut fixture = TestFixture::setup_with_plan(
            slots_in_epoch,
            current_slot,
            num_nodes,
            lookahead_leaders,
            refresh_every,
            3,
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
            fixture.slot_sender.send((next_slot, timestamp())).unwrap();
            sleep(Duration::from_millis(DEFAULT_MS_PER_SLOT)).await;
        }

        // the leaders are updated every `refresh_every` duration when the slot
        // changes. The small epsilon is added to ensure that the timeout is not
        // too strict.
        let change_result = timeout(
            refresh_every + Duration::from_millis(100),
            fixture.leaders_receiver.receiver.changed(),
        )
        .await;
        assert!(
            change_result.is_ok(),
            "timed out waiting for leaders update"
        );
        assert_eq!(fixture.rpc_client.get_transport_stats().request_count, 49);

        let (emitted_slot, emitted_sockets) = fixture.leaders_receiver.receiver.borrow().clone();
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
