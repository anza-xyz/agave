use {
    crate::nonblocking::qos::QosControllerWithCensor,
    crossbeam_channel::Receiver,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::Arc,
        time::{Duration, Instant},
    },
    tokio::sync::RwLock,
    tokio_util::sync::CancellationToken,
};

pub const DEFAULT_CENSOR_CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

/// Feedback sent to the QUIC streamer by consumers.
/// This can support different type of receivers.
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
                    warn!("Error during client cleanup: {}", e);
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
                                "Auto-uncensored expired client: {} (censored for {:?}, duration: {:?})",
                                pubkey,
                                now.duration_since(original_info.censored_time),
                                original_info.censor_duration
                            );
                        }
                    }
                }
            }

            if actually_removed > 0 {
                info!("Cleaned up {} expired censored clients", actually_removed);
            }

            actually_removed
        } else {
            0
        };

        Ok(removed_count)
    }

    // Add method to get statistics about censored clients
    pub(crate) async fn get_censor_stats(&self) -> CensorStats {
        let censored_map = self.censored_client.read().await;
        let now = Instant::now();

        let mut indefinite_count = 0;
        let mut temporary_count = 0;
        let mut expired_count = 0;

        for info in censored_map.values() {
            if let Some(duration) = info.censor_duration {
                if now.duration_since(info.censored_time) >= duration {
                    expired_count += 1;
                } else {
                    temporary_count += 1;
                }
            } else {
                indefinite_count += 1;
            }
        }

        CensorStats {
            total_censored: censored_map.len(),
            indefinite_count,
            temporary_count,
            expired_count, // These should be cleaned up soon
        }
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
            let mut censored_client = self.censored_client.write().await;
            censored_client.insert(
                client.clone(),
                ClientCensorInfo {
                    censored_time: Instant::now(),
                    censor_duration: duration,
                },
            );
            debug!("Censoring client: {}", client);
            drop(censored_client);
        }
        self.qos.censor_client(client).await;
    }

    pub(crate) async fn is_client_censored(&self, client: &Pubkey) -> bool {
        let censored_client = self.censored_client.read().await;
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
                    continue;
                }
                crossbeam_channel::RecvTimeoutError::Disconnected => {
                    break;
                }
            },
        }
    }
}

// Statistics structure
#[derive(Debug, Clone)]
pub struct CensorStats {
    pub total_censored: usize,
    pub indefinite_count: usize,
    pub temporary_count: usize,
    pub expired_count: usize, // Should be 0 if cleanup is working properly
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
