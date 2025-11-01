use {
    crate::nonblocking::qos::QosControllerWithCensor,
    crossbeam_channel::Receiver,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    },
    tokio::sync::RwLock,
    tokio_util::sync::CancellationToken,
};

pub const DEFAULT_CENSOR_CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

/// Feedback sent to the QUIC streamer by consumers.
/// This can support different type of receivers.
#[derive(Debug)]
pub enum StreamerFeedback {
    /// Censor the pubkey for an optional duration
    /// If duration is None, censor indefinitely, unless
    /// it is uncensored later explicitly. If duration is Some,
    /// censor for that duration and the censor will be removed
    /// automatically.
    CensorClient((Pubkey, Option<Duration>)),
    // Uncensor the pubkey
    UncensorClient(Pubkey),
}
#[derive(Debug, Clone)] // Add Clone here
struct ClientCensorInfo {
    censored_time: Instant,
    censor_duration: Option<Duration>,
}

pub(crate) struct FeedbackManager<Q>
where
    Q: QosControllerWithCensor,
{
    censored_client: Arc<RwLock<HashMap<Pubkey, ClientCensorInfo>>>,
    qos: Arc<Q>,
    cleanup_handle: tokio::task::JoinHandle<()>, // Store handle to cancel if needed
}

impl<Q> FeedbackManager<Q>
where
    Q: QosControllerWithCensor,
{
    pub(crate) fn new(qos: Arc<Q>) -> Self {
        Self::new_with_cleanup_interval(qos, DEFAULT_CENSOR_CLEANUP_INTERVAL)
    }

    pub(crate) fn new_with_cleanup_interval(qos: Arc<Q>, cleanup_interval: Duration) -> Self {
        let censored_client = Arc::new(RwLock::new(HashMap::new()));

        // Start the cleanup task
        let cleanup_handle = Self::start_cleanup_task(censored_client.clone(), cleanup_interval);

        Self {
            censored_client,
            qos,
            cleanup_handle,
        }
    }

    // Start the background cleanup task and return the handle
    fn start_cleanup_task(
        censored_client: Arc<RwLock<HashMap<Pubkey, ClientCensorInfo>>>,
        cleanup_interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                let cleanup_result = Self::cleanup_expired_clients(&censored_client).await;
                if let Err(e) = cleanup_result {
                    warn!("Error during client cleanup: {e}");
                }
            }
        })
    }

    // Enhanced cleanup with error handling and statistics
    async fn cleanup_expired_clients(
        censored_client: &Arc<RwLock<HashMap<Pubkey, ClientCensorInfo>>>,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let now = Instant::now();
        let mut clients_to_remove = Vec::new();

        // First pass: identify expired clients
        {
            let censored_map = censored_client.read().await;
            for (pubkey, info) in censored_map.iter() {
                if let Some(duration) = info.censor_duration {
                    if now.duration_since(info.censored_time) >= duration {
                        // Clone both the pubkey and the info to avoid borrowing issues
                        clients_to_remove.push((*pubkey, info.clone())); // *pubkey dereferences to Pubkey (which is Copy)
                    }
                }
            }
        } // censored_map is dropped here, but we've cloned the data we need

        // Second pass: remove expired clients
        let removed_count = if !clients_to_remove.is_empty() {
            let mut censored_map = censored_client.write().await;
            let mut actually_removed = 0;

            for (pubkey, original_info) in &clients_to_remove {
                // Double-check the client is still expired (in case it was updated between reads)
                if let Some(current_info) = censored_map.get(pubkey) {
                    if let Some(duration) = current_info.censor_duration {
                        if now.duration_since(current_info.censored_time) >= duration {
                            censored_map.remove(pubkey);
                            actually_removed += 1;

                            debug!(
                                "Auto-uncensored expired client: {} (censored for {:?}, duration: \
                                 {:?})",
                                pubkey,
                                now.duration_since(original_info.censored_time),
                                original_info.censor_duration
                            );
                        }
                    }
                }
            }

            if actually_removed > 0 {
                info!("Cleaned up {actually_removed} expired censored clients");
            }

            actually_removed
        } else {
            0
        };

        Ok(removed_count)
    }

    pub(crate) async fn handle_feedback(&self, feedback: StreamerFeedback) {
        match feedback {
            StreamerFeedback::CensorClient((address, duration)) => {
                self.censor_client(&address, duration).await;
            }
            StreamerFeedback::UncensorClient(address) => {
                self.uncensor_client(&address).await;
            }
        }
    }

    pub(crate) async fn uncensor_client(&self, client: &Pubkey) {
        let mut censored_client = self.censored_client.write().await;
        let removed_client = censored_client.remove(client);
        if let Some(removed_client) = removed_client {
            debug!(
                "Uncensoring with initial censor_time: {:?} pubkey: {client:?}",
                removed_client.censored_time
            );
        }
    }

    pub(crate) async fn censor_client(&self, client: &Pubkey, duration: Option<Duration>) {
        {
            debug!(
                "Censoring client: {} for duration: {:?} {:p}",
                client, duration, self
            );
            let mut censored_client = self.censored_client.write().await;
            censored_client.insert(
                *client,
                ClientCensorInfo {
                    censored_time: Instant::now(),
                    censor_duration: duration,
                },
            );
            debug!(
                "Censoring client: {}, censored_keys: {:?}",
                client,
                censored_client.keys()
            );
            drop(censored_client);
        }
        self.qos.censor_client(client).await;
    }

    pub(crate) async fn is_client_censored(&self, client: &Pubkey) -> bool {
        let censored_client = self.censored_client.read().await;
        debug!(
            "Checking if client is censored: {} censored_keys: {:?} {:p}",
            client,
            censored_client.keys(),
            self
        );
        censored_client.contains_key(client)
    }
}

pub(crate) async fn run_feedback_receiver<Q>(
    feedback_manager: Arc<FeedbackManager<Q>>,
    feedback_receiver: Receiver<StreamerFeedback>,
    cancel: CancellationToken,
) where
    Q: QosControllerWithCensor,
{
    let feedback_timeout = Duration::from_secs(1);
    info!("Running feedback receiver");
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let feedback = feedback_receiver.recv_timeout(feedback_timeout);
        match feedback {
            Ok(feedback) => {
                feedback_manager.handle_feedback(feedback).await;
            }
            Err(error) => match error {
                crossbeam_channel::RecvTimeoutError::Timeout => {
                    debug!("Feedback receiver timeout, checking for cancellation");
                    continue;
                }
                crossbeam_channel::RecvTimeoutError::Disconnected => {
                    break;
                }
            },
        }
    }
}

// Keep only this:
#[derive(Debug, Default)]
pub struct CensoringStats {
    pub total_censored: AtomicUsize,
    pub indefinite_count: AtomicUsize,
    pub temporary_count: AtomicUsize,
    pub expired_count: AtomicUsize,
    pub total_censored_events: AtomicUsize,
    pub total_uncensored_events: AtomicUsize,
    pub auto_cleanup_runs: AtomicUsize,
    pub clients_auto_uncensored: AtomicUsize,
}

impl CensoringStats {
    // Remove the snapshot method, access fields directly
    pub fn total_censored(&self) -> usize {
        self.total_censored.load(Ordering::Relaxed)
    }

    pub fn indefinite_count(&self) -> usize {
        self.indefinite_count.load(Ordering::Relaxed)
    }

    pub fn temporary_count(&self) -> usize {
        self.temporary_count.load(Ordering::Relaxed)
    }

    pub fn expired_count(&self) -> usize {
        self.expired_count.load(Ordering::Relaxed)
    }

    pub fn total_censored_events(&self) -> usize {
        self.total_censored_events.load(Ordering::Relaxed)
    }

    pub fn total_uncensored_events(&self) -> usize {
        self.total_uncensored_events.load(Ordering::Relaxed)
    }

    pub fn auto_cleanup_runs(&self) -> usize {
        self.auto_cleanup_runs.load(Ordering::Relaxed)
    }

    pub fn clients_auto_uncensored(&self) -> usize {
        self.clients_auto_uncensored.load(Ordering::Relaxed)
    }

    // Or keep snapshot but return the same type
    pub fn get_current_stats(&self) -> CensoringStats {
        CensoringStats {
            total_censored: AtomicUsize::new(self.total_censored.load(Ordering::Relaxed)),
            indefinite_count: AtomicUsize::new(self.indefinite_count.load(Ordering::Relaxed)),
            temporary_count: AtomicUsize::new(self.temporary_count.load(Ordering::Relaxed)),
            expired_count: AtomicUsize::new(self.expired_count.load(Ordering::Relaxed)),
            total_censored_events: AtomicUsize::new(
                self.total_censored_events.load(Ordering::Relaxed),
            ),
            total_uncensored_events: AtomicUsize::new(
                self.total_uncensored_events.load(Ordering::Relaxed),
            ),
            auto_cleanup_runs: AtomicUsize::new(self.auto_cleanup_runs.load(Ordering::Relaxed)),
            clients_auto_uncensored: AtomicUsize::new(
                self.clients_auto_uncensored.load(Ordering::Relaxed),
            ),
        }
    }
}

// Clean shutdown support
impl<Q> Drop for FeedbackManager<Q>
where
    Q: QosControllerWithCensor,
{
    fn drop(&mut self) {
        self.cleanup_handle.abort(); // Cancel the cleanup task when FeedbackManager is dropped
    }
}
