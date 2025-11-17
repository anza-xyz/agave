//! This module provides [`LeaderTpuCacheService`] structure along with [`LeaderUpdateReceiver`],
//! [`Config`]. [`LeaderTpuCacheService`] tracks the current and upcoming Solana leader nodes and
//! their TPU socket addresses.
#![allow(clippy::arithmetic_side_effects)]
use {
    crate::{
        connection_workers_scheduler::extract_send_leaders,
        logging::{debug, error, info, warn},
        node_address_service::SlotReceiver,
    },
    async_trait::async_trait,
    solana_clock::{Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_commitment_config::CommitmentConfig,
    solana_pubkey::Pubkey,
    solana_quic_definitions::QUIC_PORT_OFFSET,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{client_error::Error as ClientError, response::RpcContactInfo},
    std::{
        collections::HashMap, future::Future, net::SocketAddr, str::FromStr, sync::Arc,
        time::Instant,
    },
    thiserror::Error,
    tokio::{
        sync::watch,
        task::JoinHandle,
        time::{interval, Duration},
    },
    tokio_util::sync::CancellationToken,
};

/// Maximum number of slots used to build TPU socket fanout set
const MAX_FANOUT_SLOTS: u64 = 100;

/// Configuration for the [`LeaderTpuCacheService`].
#[derive(Debug, Clone)]
pub struct Config {
    /// max number of leaders to look ahead for, not necessary unique.
    pub lookahead_leaders: u8,
    /// how often to refresh cluster nodes info.
    pub refresh_nodes_info_every: Duration,
    /// maximum number of consecutive failures to tolerate.
    pub max_consecutive_failures: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lookahead_leaders: 1,
            refresh_nodes_info_every: Duration::from_secs(5 * 60),
            max_consecutive_failures: 10,
        }
    }
}

/// [`LeaderTpuCacheService`] is a background task that tracks the current and upcoming Solana
/// leader nodes and updates their TPU socket addresses encapsulated in [`LeaderUpdateReceiver`] for
/// downstream consumers.
pub struct LeaderTpuCacheService {
    handle: Option<JoinHandle<Result<(), Error>>>,
    cancel: CancellationToken,
}

/// Receiver for leader TPU socket address updates from
/// [`LeaderTpuCacheService`].
#[derive(Clone)]
pub struct LeaderUpdateReceiver {
    receiver: watch::Receiver<NodesTpuInfo>,
}

impl LeaderUpdateReceiver {
    pub fn leaders(&self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        let NodesTpuInfo { leaders, extend } = self.receiver.borrow().clone();
        let lookahead_leaders = if extend {
            lookahead_leaders.saturating_add(1)
        } else {
            lookahead_leaders
        };
        extract_send_leaders(&leaders, lookahead_leaders)
    }
}

/// [`NodesTpuInfo`] holds the TPU addresses of the nodes scheduled to be leaders for upcoming
/// slots. The `extend` flag indicates whether the list of leaders was extended by one to account
/// for the case when the current slot is the last slot in a leader's consecutive slots.
#[derive(Clone)]
struct NodesTpuInfo {
    leaders: Vec<SocketAddr>,
    extend: bool,
}

impl LeaderTpuCacheService {
    /// Run the [`LeaderTpuCacheService`], returning receiver and the service.
    pub async fn run(
        cluster_info: Arc<impl ClusterInfoProvider + 'static>,
        slot_receiver: SlotReceiver,
        config: Config,
        cancel: CancellationToken,
    ) -> Result<(LeaderUpdateReceiver, Self), Error> {
        let (leader_tpu_map, epoch_info, slot_leaders) = initialize_state(
            cluster_info.as_ref(),
            slot_receiver.clone(),
            config.max_consecutive_failures,
        )
        .await?;
        let current_slot = slot_receiver.slot();
        let lookahead_leaders =
            adjust_lookahead(current_slot, &slot_leaders, config.lookahead_leaders);
        let leaders = leader_sockets(
            current_slot,
            lookahead_leaders,
            &slot_leaders,
            &leader_tpu_map,
        );

        let (leaders_sender, leaders_receiver) = watch::channel(NodesTpuInfo {
            leaders,
            extend: config.lookahead_leaders != lookahead_leaders,
        });

        let handle = tokio::spawn(Self::run_loop(
            cluster_info,
            slot_receiver,
            epoch_info,
            slot_leaders,
            leader_tpu_map,
            config,
            leaders_sender,
            cancel.clone(),
        ));

        Ok((
            LeaderUpdateReceiver {
                receiver: leaders_receiver,
            },
            Self {
                handle: Some(handle),
                cancel,
            },
        ))
    }

    /// Gracefully shutdown the [`LeaderTpuCacheService`].
    pub async fn shutdown(&mut self) -> Result<(), Error> {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.await??;
        }
        Ok(())
    }

    async fn run_loop(
        cluster_info: Arc<impl ClusterInfoProvider + 'static>,
        mut slot_receiver: SlotReceiver,
        mut epoch_info: EpochInfo,
        mut slot_leaders: SlotLeaders,
        mut leader_tpu_map: LeaderTpuMap,
        config: Config,
        leaders_sender: watch::Sender<NodesTpuInfo>,
        cancel: CancellationToken,
    ) -> Result<(), Error> {
        let mut num_consecutive_failures: usize = 0;
        let mut refresh_tpu_interval = interval(config.refresh_nodes_info_every);
        loop {
            tokio::select! {
                _ = refresh_tpu_interval.tick() => {
                    try_update(
                        "cluster TPU ports",
                        &mut leader_tpu_map,
                        || LeaderTpuMap::new(cluster_info.as_ref()),
                        &mut num_consecutive_failures,
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

                    let estimated_current_slot = slot_receiver.slot();
                    update_leader_info(
                        estimated_current_slot,
                        cluster_info.as_ref(),
                        &mut epoch_info,
                        &mut slot_leaders,
                        &mut num_consecutive_failures,
                        config.max_consecutive_failures,
                    ).await?;
                    let current_slot = slot_receiver.slot();
                    let lookahead_leaders = adjust_lookahead(
                        current_slot,
                        &slot_leaders,
                        config.lookahead_leaders,
                    );
                    let leaders = leader_sockets(current_slot, lookahead_leaders, &slot_leaders, &leader_tpu_map);

                    if let Err(e) = leaders_sender.send(NodesTpuInfo { leaders, extend: config.lookahead_leaders != lookahead_leaders }) {
                        warn!("Unexpectedly dropped leaders_sender: {e}");
                        return Err(Error::ChannelClosed);
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

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    RpcError(#[from] ClientError),

    #[error("Failed to get slot leaders connecting to: {0}")]
    SlotLeadersConnectionFailed(String),

    #[error("Failed find any cluster node info for upcoming leaders, timeout: {0}")]
    ClusterNodeNotFound(String),

    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),

    #[error("Unexpectedly dropped a channel.")]
    ChannelClosed,

    #[error("Failed to initialize LeaderTpuCacheService.")]
    InitializationFailed,
}

/// [`ClusterInfoProvider`] provides information about the cluster such as epoch info, node tpu
/// addresses, and leader schedule. Beside of that it also provides the initial slot to start from
/// which is called once during initialization. All this information is required by
/// [`LeaderTpuCacheService`] to estimate the next leader.
#[async_trait]
pub trait ClusterInfoProvider: Send + Sync {
    async fn initial_slot(&self) -> Result<Slot, Error>;
    async fn tpu_socket_map(&self) -> Result<HashMap<Pubkey, SocketAddr>, Error>;
    async fn epoch_info(&self, first_slot: Slot) -> Result<(Slot, Slot), Error>;
    async fn slot_leaders(&self, first_slot: Slot, slots_limit: u64) -> Result<Vec<Pubkey>, Error>;
}

async fn update_leader_info(
    estimated_current_slot: Slot,
    cluster_info: &impl ClusterInfoProvider,
    epoch_info: &mut EpochInfo,
    slot_leaders: &mut SlotLeaders,
    num_consecutive_failures: &mut usize,
    max_consecutive_failures: usize,
) -> Result<(), Error> {
    if estimated_current_slot > epoch_info.last_slot_in_epoch {
        try_update(
            "epoch info",
            epoch_info,
            || EpochInfo::new(cluster_info, estimated_current_slot),
            num_consecutive_failures,
            max_consecutive_failures,
        )
        .await?;
    }
    if estimated_current_slot.saturating_add(MAX_FANOUT_SLOTS) > slot_leaders.last_slot() {
        try_update(
            "slot leaders",
            slot_leaders,
            || {
                SlotLeaders::new(
                    cluster_info,
                    estimated_current_slot,
                    epoch_info.slots_in_epoch,
                )
            },
            num_consecutive_failures,
            max_consecutive_failures,
        )
        .await?;
    }
    Ok(())
}

/// Get the TPU sockets for slots starting from `first_slot` and until `first_slot +
/// lookahead_leaders * NUM_CONSECUTIVE_LEADER_SLOTS`.
///
/// If it returns an empty vector, it might mean that we overran the local leader schedule cache or,
/// less probable, that there is no TPU info available for corresponding slot leaders.
fn leader_sockets(
    first_slot: Slot,
    lookahead_leaders: u8,
    slot_leaders: &SlotLeaders,
    leader_tpu_map: &LeaderTpuMap,
) -> Vec<SocketAddr> {
    let fanout_slots = (lookahead_leaders as u64).saturating_mul(NUM_CONSECUTIVE_LEADER_SLOTS);
    let mut leader_sockets = Vec::with_capacity(lookahead_leaders as usize);
    // `slot_leaders.first_slot` might have been advanced since caller last read it. Take the
    // greater of the two values to ensure we are reading from the latest leader schedule.
    let current_slot = std::cmp::max(first_slot, slot_leaders.first_slot);
    for leader_slot in
        (current_slot..current_slot + fanout_slots).step_by(NUM_CONSECUTIVE_LEADER_SLOTS as usize)
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
    cluster_info: &impl ClusterInfoProvider,
    slot_receiver: SlotReceiver,
    max_attempts: usize,
) -> Result<(LeaderTpuMap, EpochInfo, SlotLeaders), Error> {
    const ATTEMPTS_SLEEP_DURATION: Duration = Duration::from_millis(100);
    let mut leader_tpu_map = None;
    let mut epoch_info = None;
    let mut slot_leaders = None;
    let mut num_attempts: usize = 0;
    while num_attempts < max_attempts {
        let iteration_start = Instant::now();
        if leader_tpu_map.is_none() {
            leader_tpu_map = LeaderTpuMap::new(cluster_info).await.ok();
        }
        if epoch_info.is_none() {
            epoch_info = EpochInfo::new(cluster_info, slot_receiver.slot())
                .await
                .ok();
        }

        if let Some(epoch_info) = &epoch_info {
            if slot_leaders.is_none() {
                slot_leaders = SlotLeaders::new(
                    cluster_info,
                    slot_receiver.slot(),
                    epoch_info.slots_in_epoch,
                )
                .await
                .ok();
            }
        }
        if leader_tpu_map.is_some() && epoch_info.is_some() && slot_leaders.is_some() {
            return Ok((
                leader_tpu_map.take().unwrap(),
                epoch_info.take().unwrap(),
                slot_leaders.take().unwrap(),
            ));
        }
        num_attempts += 1;

        let elapsed = iteration_start.elapsed();
        if elapsed < ATTEMPTS_SLEEP_DURATION {
            tokio::time::sleep(ATTEMPTS_SLEEP_DURATION - elapsed).await;
        }
    }
    Err(Error::InitializationFailed)
}

fn adjust_lookahead(slot: Slot, slot_leaders: &SlotLeaders, lookahead_leaders: u8) -> u8 {
    if slot_leaders
        .is_leader_last_consecutive_slot(slot)
        .unwrap_or(true)
    {
        lookahead_leaders.saturating_add(1)
    } else {
        lookahead_leaders
    }
}

async fn try_update<F, Fut, T>(
    label: &str,
    data: &mut T,
    make_call: F,
    num_failures: &mut usize,
    max_failures: usize,
) -> Result<(), Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
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

#[derive(Debug)]
struct LeaderTpuMap {
    leader_tpu_map: HashMap<Pubkey, SocketAddr>,
}

impl LeaderTpuMap {
    async fn new(cluster_info: &impl ClusterInfoProvider) -> Result<Self, Error> {
        let leader_tpu_map = cluster_info.tpu_socket_map().await?;
        Ok(Self { leader_tpu_map })
    }

    fn get(&self, leader: &Pubkey) -> Option<&SocketAddr> {
        self.leader_tpu_map.get(leader)
    }
}

/// Structure [`SlotLeaders`] provides a view on the leaders schedule starting from `first_slot`.
#[derive(PartialEq, Debug)]
struct SlotLeaders {
    first_slot: Slot,
    leaders: Vec<Pubkey>,
}

impl SlotLeaders {
    /// Creates a new [`SlotLeaders`] instance by fetching slot leaders up to `slots_limit`.
    ///
    /// Note, that if it managed to fetch less slot leaders than requested, it will still succeed.
    async fn new(
        cluster_info: &impl ClusterInfoProvider,
        first_slot: Slot,
        slots_limit: u64,
    ) -> Result<Self, Error> {
        Ok(Self {
            first_slot,
            leaders: cluster_info.slot_leaders(first_slot, slots_limit).await?,
        })
    }

    fn last_slot(&self) -> Slot {
        self.first_slot + self.leaders.len().saturating_sub(1) as u64
    }

    fn slot_leader(&self, slot: Slot) -> Option<&Pubkey> {
        slot.checked_sub(self.first_slot)
            .and_then(|index| self.leaders.get(index as usize))
    }

    /// Returns `Some(true)` if the given `slot` is the last slot in the leader consecutive slots.
    fn is_leader_last_consecutive_slot(&self, slot: Slot) -> Option<bool> {
        slot.checked_sub(self.first_slot).and_then(|index| {
            let index = index as usize;
            if index + 1 < self.leaders.len() {
                Some(self.leaders[index] != self.leaders[index + 1])
            } else {
                None
            }
        })
    }
}

#[derive(PartialEq, Debug)]
struct EpochInfo {
    slots_in_epoch: Slot,
    last_slot_in_epoch: Slot,
}

impl EpochInfo {
    async fn new(cluster_info: &impl ClusterInfoProvider, first_slot: Slot) -> Result<Self, Error> {
        let (slots_in_epoch, last_slot_in_epoch) = cluster_info.epoch_info(first_slot).await?;
        Ok(Self {
            slots_in_epoch,
            last_slot_in_epoch,
        })
    }
}

#[async_trait]
impl ClusterInfoProvider for RpcClient {
    async fn initial_slot(&self) -> Result<Slot, Error> {
        self.get_slot_with_commitment(CommitmentConfig::processed())
            .await
            .map_err(Error::RpcError)
    }

    async fn tpu_socket_map(&self) -> Result<HashMap<Pubkey, SocketAddr>, Error> {
        let cluster_nodes = self.get_cluster_nodes().await.map_err(Error::RpcError)?;
        Ok(extract_cluster_tpu_sockets(cluster_nodes))
    }

    async fn epoch_info(&self, first_slot: Slot) -> Result<(Slot, Slot), Error> {
        let epoch_schedule = self.get_epoch_schedule().await.map_err(Error::RpcError)?;
        let epoch = epoch_schedule.get_epoch(first_slot);
        let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
        let last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);
        debug!(
            "Updated slots in epoch: {slots_in_epoch}, last slot in epoch: {last_slot_in_epoch}",
        );
        Ok((slots_in_epoch, last_slot_in_epoch))
    }

    /// Returns the slot leaders starting from `first_slot` until `first_slot + slots_limit`.
    ///
    /// Partial results may be returned if `slots_limit` exceeds the maximum number of slots.
    async fn slot_leaders(&self, first_slot: Slot, slots_limit: u64) -> Result<Vec<Pubkey>, Error> {
        // `2` is used to avoid refetching the leaders until the middle of the requested range.
        let max_slots_to_fetch = (2 * MAX_FANOUT_SLOTS).min(slots_limit);
        let slot_leaders = self.get_slot_leaders(first_slot, max_slots_to_fetch).await;
        debug!("Fetched slot leaders from slot {first_slot} for {slots_limit}. ");
        slot_leaders.map_err(Error::RpcError)
    }
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
