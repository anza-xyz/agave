//! This module provides [`LeaderTpuCacheService`] structure along with
//! [`LeaderUpdateReceiver`], [`LeaderTpuCacheServiceConfig`].
//! [`LeaderTpuCacheService`] tracks the current and upcoming Solana leader
//! nodes and their TPU socket addresses.
#![allow(clippy::arithmetic_side_effects)]
use {
    crate::{
        connection_workers_scheduler::extract_send_leaders,
        logging::{debug, error, info, warn},
        node_address_service::{slot_receiver::EstimatedSlot, SlotReceiver},
    },
    async_trait::async_trait,
    solana_clock::{Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_pubkey::Pubkey,
    solana_quic_definitions::QUIC_PORT_OFFSET,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::client_error::Error as ClientError,
    solana_rpc_client_api::response::RpcContactInfo,
    std::{collections::HashMap, future::Future, net::SocketAddr, str::FromStr, sync::Arc},
    thiserror::Error,
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
    /// max number of leaders to look ahead for, not necessary unique.
    pub lookahead_leaders: u64,
    /// how often to refresh cluster nodes info.
    pub refresh_nodes_info_every: Duration,
    /// maximum number of consecutive failures to tolerate.
    pub max_consecutive_failures: usize,
}

/// [`LeaderTpuCacheService`] is a background task that tracks the current and
/// upcoming Solana leader nodes and updates their TPU socket addresses
/// encapsulated in [`LeaderUpdateReceiver`] for downstream consumers.
pub struct LeaderTpuCacheService {
    handle: Option<JoinHandle<Result<(), LeaderTpuCacheServiceError>>>,
    cancel: CancellationToken,
}

/// Receiver for leader TPU socket address updates from
/// [`LeaderTpuCacheService`].
#[derive(Clone)]
pub struct LeaderUpdateReceiver {
    receiver: watch::Receiver<(Vec<SocketAddr>, bool)>,
}

impl LeaderUpdateReceiver {
    pub fn next_leaders(&self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        let (leaders, extended) = self.receiver.borrow().clone();
        let lookahead_leaders = if extended {
            lookahead_leaders.saturating_add(1)
        } else {
            lookahead_leaders
        };
        extract_send_leaders(&leaders, lookahead_leaders)
    }
}

impl LeaderTpuCacheService {
    /// Run the [`LeaderTpuCacheService`], returning receiver and the service.
    pub async fn run(
        rpc_client: Arc<impl ClusterInfoProvider + 'static>,
        mut slot_receiver: SlotReceiver,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<(LeaderUpdateReceiver, Self), LeaderTpuCacheServiceError> {
        let (mut leader_tpu_map, mut epoch_info, mut slot_leaders) = Self::initialize_state(
            rpc_client.as_ref(),
            slot_receiver.clone(),
            config.max_consecutive_failures,
        )
        .await?;
        let (current_slot, lookahead_leaders) = get_slot_and_lookahead(
            slot_receiver.slot(),
            &slot_leaders,
            config.lookahead_leaders,
        );
        let leaders = Self::leader_sockets(
            current_slot,
            lookahead_leaders,
            &slot_leaders,
            &leader_tpu_map,
        );

        let (leaders_sender, leaders_receiver) =
            watch::channel((leaders, config.lookahead_leaders != lookahead_leaders));

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

                        let estimated_current_slot = slot_receiver.slot().first_slot();
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

                        let (current_slot, lookahead_leaders) = get_slot_and_lookahead(
                            slot_receiver.slot(),
                            &slot_leaders,
                            config.lookahead_leaders,
                        );
                        let leaders = Self::leader_sockets(current_slot, lookahead_leaders, &slot_leaders, &leader_tpu_map);

                        if let Err(e) = leaders_sender.send((leaders, config.lookahead_leaders != lookahead_leaders)) {
                            warn!("Unexpectedly dropped leaders_sender: {e}");
                            return Err(LeaderTpuCacheServiceError::ChannelClosed);
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
                handle: Some(handle),
                cancel: cancel_clone,
            },
        ))
    }

    /// Gracefully shutdown the [`LeaderTpuCacheService`].
    pub async fn shutdown(&mut self) -> Result<(), LeaderTpuCacheServiceError> {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.await??;
        }
        Ok(())
    }

    /// Get the TPU sockets for the current and upcoming leaders according to
    /// fanout size.
    fn leader_sockets(
        estimated_current_slot: Slot,
        lookahead_leaders: u64,
        slot_leaders: &SlotLeaders,
        leader_tpu_map: &LeaderTpuMap,
    ) -> Vec<SocketAddr> {
        let fanout_slots = lookahead_leaders.saturating_mul(NUM_CONSECUTIVE_LEADER_SLOTS);
        let mut leader_sockets = Vec::with_capacity(lookahead_leaders as usize);
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
    ) -> Result<(LeaderTpuMap, EpochInfo, SlotLeaders), LeaderTpuCacheServiceError> {
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
                epoch_info = EpochInfo::new(rpc_client, slot_receiver.slot().first_slot())
                    .await
                    .ok();
            }

            if let Some(epoch_info) = &epoch_info {
                if slot_leaders.is_none() {
                    slot_leaders = SlotLeaders::new(
                        rpc_client,
                        slot_receiver.slot().first_slot(),
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
            Err(LeaderTpuCacheServiceError::InitializationFailed)
        } else {
            Ok((
                leader_tpu_map.unwrap(),
                epoch_info.unwrap(),
                slot_leaders.unwrap(),
            ))
        }
    }
}

#[derive(Debug, Error)]
pub enum LeaderTpuCacheServiceError {
    #[error(transparent)]
    RpcError(#[from] ClientError),

    #[error("Failed to get slot leaders connecting to: {0}")]
    SlotLeadersConnectionFailed(String),

    #[error("Failed find any cluster node info for upcoming leaders, timeout: {0}")]
    ClusterNodeNotFound(String),

    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),

    #[error("Unexpectly dropped a channel.")]
    ChannelClosed,

    #[error("Failed to initialize LeaderTpuCacheService.")]
    InitializationFailed,
}

#[async_trait]
pub trait ClusterInfoProvider: Send + Sync {
    async fn leader_tpu_map(
        &self,
    ) -> Result<HashMap<Pubkey, SocketAddr>, LeaderTpuCacheServiceError>;
    async fn epoch_info(
        &self,
        estimated_current_slot: Slot,
    ) -> Result<(Slot, Slot), LeaderTpuCacheServiceError>;
    async fn slot_leaders(
        &self,
        estimated_current_slot: Slot,
        slots_in_epoch: Slot,
    ) -> Result<Vec<Pubkey>, LeaderTpuCacheServiceError>;
}

fn get_slot_and_lookahead(
    estimated_slot: EstimatedSlot,
    slot_leaders: &SlotLeaders,
    lookahead_leaders: u64,
) -> (Slot, u64) {
    match estimated_slot {
        EstimatedSlot::Single(slot) => {
            if slot_leaders.is_last_slot_in_window(slot).unwrap_or(false) {
                (slot, lookahead_leaders.saturating_add(1))
            } else {
                (slot, lookahead_leaders)
            }
        }
        EstimatedSlot::Multiple([slot, _]) => (slot, lookahead_leaders.saturating_add(1)),
    }
}

async fn try_update<F, Fut, T>(
    label: &str,
    data: &mut T,
    make_call: F,
    num_failures: &mut usize,
    max_failures: usize,
) -> Result<(), LeaderTpuCacheServiceError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, LeaderTpuCacheServiceError>>,
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
    async fn new(
        rpc_client: &impl ClusterInfoProvider,
    ) -> Result<Self, LeaderTpuCacheServiceError> {
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
    ) -> Result<Self, LeaderTpuCacheServiceError> {
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

    fn is_last_slot_in_window(&self, slot: Slot) -> Option<bool> {
        if slot >= self.first_slot {
            let index = (slot - self.first_slot) as usize;
            if index + 1 < self.leaders.len() {
                Some(self.leaders[index] != self.leaders[index + 1])
            } else {
                None
            }
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
    ) -> Result<Self, LeaderTpuCacheServiceError> {
        let (slots_in_epoch, last_slot_in_epoch) =
            rpc_client.epoch_info(estimated_current_slot).await?;
        Ok(Self {
            slots_in_epoch,
            last_slot_in_epoch,
        })
    }
}

#[async_trait]
impl ClusterInfoProvider for RpcClient {
    async fn leader_tpu_map(
        &self,
    ) -> Result<HashMap<Pubkey, SocketAddr>, LeaderTpuCacheServiceError> {
        let cluster_nodes = self.get_cluster_nodes().await;
        match cluster_nodes {
            Ok(cluster_nodes) => Ok(extract_cluster_tpu_sockets(cluster_nodes)),
            Err(err) => Err(LeaderTpuCacheServiceError::RpcError(err)),
        }
    }

    async fn epoch_info(
        &self,
        estimated_current_slot: Slot,
    ) -> Result<(Slot, Slot), LeaderTpuCacheServiceError> {
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
            Err(err) => Err(LeaderTpuCacheServiceError::RpcError(err)),
        }
    }

    async fn slot_leaders(
        &self,
        estimated_current_slot: Slot,
        slots_in_epoch: Slot,
    ) -> Result<Vec<Pubkey>, LeaderTpuCacheServiceError> {
        let slot_leaders = self
            .get_slot_leaders(estimated_current_slot, fanout(slots_in_epoch))
            .await;
        debug!("Fetched slot leaders from slot {estimated_current_slot} for {slots_in_epoch}. ");
        match slot_leaders {
            Ok(slot_leaders) => Ok(slot_leaders),
            Err(err) => Err(LeaderTpuCacheServiceError::RpcError(err)),
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
