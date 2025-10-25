//! This module defines [`ConnectionWorker`] which encapsulates the functionality
//! needed to handle one connection within the scope of task.

use {
    super::SendTransactionStats,
    crate::{
        logging::{debug, error, trace, warn},
        quic_networking::send_data_over_stream,
        send_transaction_stats::record_error,
        transaction_batch::TransactionBatch,
        QuicError,
    },
    quinn::{ConnectError, Connection, ConnectionError, Endpoint},
    solana_clock::{DEFAULT_MS_PER_SLOT, MAX_PROCESSING_AGE, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_measure::measure::Measure,
    solana_time_utils::timestamp,
    solana_tls_utils::socket_addr_to_quic_server_name,
    std::{
        net::SocketAddr,
        sync::{atomic::Ordering, Arc},
    },
    tokio::{
        sync::mpsc,
        time::{sleep, timeout, Duration},
    },
    tokio_util::sync::CancellationToken,
};

/// The maximum connection handshake timeout for QUIC connections.
/// This is set to 2 seconds, which was the earlier shorter connection idle timeout
/// which was also used by QUINN to timeout connection handshake.
pub(crate) const DEFAULT_MAX_CONNECTION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// Interval between retry attempts for creating a new connection. This value is
/// a best-effort estimate, based on current network conditions.
const RETRY_SLEEP_INTERVAL: Duration =
    Duration::from_millis(NUM_CONSECUTIVE_LEADER_SLOTS * DEFAULT_MS_PER_SLOT);

/// Maximum age (in milliseconds) of a blockhash, beyond which transaction
/// batches are dropped.
const MAX_PROCESSING_AGE_MS: u64 = MAX_PROCESSING_AGE as u64 * DEFAULT_MS_PER_SLOT;

/// [`ConnectionState`] represents the current state of a quic connection.
///
/// It tracks the lifecycle of connection from initial setup to closing phase.
/// The transition function between states is defined in `ConnectionWorker`
/// implementation.
enum ConnectionState {
    NotSetup,
    Active(Connection),
    Retry(usize),
    Closing,
}

impl Drop for ConnectionState {
    /// When [`ConnectionState`] is dropped, underlying connection is closed
    /// which means that there is no guarantee that the open streams will
    /// finish.
    fn drop(&mut self) {
        if let Self::Active(connection) = self {
            debug!(
                "Close connection with {:?}, stats: {:?}. All pending streams will be dropped.",
                connection.remote_address(),
                connection.stats()
            );
            connection.close(0u32.into(), b"done");
        }
    }
}

/// [`ConnectionWorker`] holds connection to the validator with address `peer`.
///
/// The worker proactively monitors connection health while processing
/// transactions, detecting connection closures immediately rather than waiting
/// for send failures.
/// If connection has been closed, [`ConnectionWorker`] tries to reconnect
/// `max_reconnect_attempts` times. If connection is in `Active` state, it sends
/// transactions received from `transactions_receiver`. Additionally, it
/// accumulates statistics about connections and streams failures.
pub(crate) struct ConnectionWorker {
    endpoint: Endpoint,
    peer: SocketAddr,
    transactions_receiver: mpsc::Receiver<TransactionBatch>,
    connection: ConnectionState,
    skip_check_transaction_age: bool,
    max_reconnect_attempts: usize,
    send_txs_stats: Arc<SendTransactionStats>,
    cancel: CancellationToken,
    handshake_timeout: Duration,
}

impl ConnectionWorker {
    /// Constructs a [`ConnectionWorker`].
    ///
    /// [`ConnectionWorker`] maintains a connection to a `peer` and processes
    /// transactions from `transactions_receiver`. If
    /// `skip_check_transaction_age` is set to `true`, the worker skips checking
    /// for transaction blockhash expiration. The `max_reconnect_attempts`
    /// parameter controls how many times the worker will attempt to reconnect
    /// in case of connection failure. Returns the created `ConnectionWorker`
    /// along with a cancellation token that can be used by the caller to stop
    /// the worker.
    pub fn new(
        endpoint: Endpoint,
        peer: SocketAddr,
        transactions_receiver: mpsc::Receiver<TransactionBatch>,
        skip_check_transaction_age: bool,
        max_reconnect_attempts: usize,
        send_txs_stats: Arc<SendTransactionStats>,
        handshake_timeout: Duration,
    ) -> (Self, CancellationToken) {
        let cancel = CancellationToken::new();
        let this = Self {
            endpoint,
            peer,
            transactions_receiver,
            connection: ConnectionState::NotSetup,
            skip_check_transaction_age,
            max_reconnect_attempts,
            send_txs_stats,
            cancel: cancel.clone(),
            handshake_timeout,
        };

        (this, cancel)
    }

    /// Starts the main loop of the [`ConnectionWorker`].
    ///
    /// This method manages the connection to the peer and handles state
    /// transitions. It runs indefinitely until the connection is closed or an
    /// unrecoverable error occurs. The worker monitors both incoming transactions
    /// and connection health simultaneously when in the Active state.
    pub async fn run(&mut self) {
        let cancel = self.cancel.clone();

        let main_loop = async move {
            loop {
                match &self.connection {
                    ConnectionState::Closing => {
                        break;
                    }
                    ConnectionState::NotSetup => {
                        self.create_connection(0).await;
                    }
                    ConnectionState::Active(connection) => {
                        tokio::select! {
                            // Process incoming transactions
                            transactions = self.transactions_receiver.recv() => {
                                match transactions {
                                    Some(batch) => {
                                        self.send_transactions(connection.clone(), batch).await;
                                    }
                                    None => {
                                        debug!(
                                            "Transactions sender has been dropped for peer: {}",
                                            self.peer
                                        );
                                        self.connection = ConnectionState::Closing;
                                    }
                                }
                            }

                            // Monitor connection health proactively
                            close_reason = connection.closed() => {
                                self.handle_connection_closed(close_reason);
                                continue;
                            }
                        }
                    }
                    ConnectionState::Retry(num_reconnects) => {
                        if *num_reconnects > self.max_reconnect_attempts {
                            error!(
                                "Failed to establish connection to {}: reached max reconnect \
                                 attempts",
                                self.peer
                            );
                            self.connection = ConnectionState::Closing;
                            continue;
                        }
                        sleep(RETRY_SLEEP_INTERVAL).await;
                        self.reconnect(*num_reconnects).await;
                    }
                }
            }
        };

        tokio::select! {
            () = main_loop => (),
            () = cancel.cancelled() => (),
        }
        // Cancel it additionally here so that in WorkerInfo we can check if
        // this worker is active.
        cancel.cancel();
    }

    /// Handles connection closure events detected by the connection monitor.
    ///
    /// This method logs the close reason with appropriate severity based on
    /// the type of closure, records statistics, and determines whether to
    /// attempt reconnection based on the error type.
    fn handle_connection_closed(&mut self, close_reason: ConnectionError) {
        match &close_reason {
            ConnectionError::ConnectionClosed(close) => {
                debug!(
                    "Connection to {} closed by peer: code={} reason={:?}",
                    self.peer,
                    close.error_code,
                    String::from_utf8_lossy(&close.reason)
                );
            }
            ConnectionError::ApplicationClosed(close) => {
                debug!(
                    "Connection to {} closed by application: code={} reason={:?}",
                    self.peer,
                    close.error_code,
                    String::from_utf8_lossy(&close.reason)
                );
            }
            ConnectionError::LocallyClosed => {
                debug!("Connection to {} closed locally", self.peer);
            }
            ConnectionError::TimedOut => {
                warn!("Connection to {} timed out", self.peer);
            }
            ConnectionError::Reset => {
                warn!("Connection to {} reset", self.peer);
            }
            ConnectionError::TransportError(e) => {
                warn!(
                    "Connection to {} encountered transport error: {}",
                    self.peer, e
                );
            }
            ConnectionError::VersionMismatch => {
                error!("Connection to {} failed: version mismatch", self.peer);
            }
            ConnectionError::CidsExhausted => {
                warn!(
                    "Connection to {} closed: connection IDs exhausted",
                    self.peer
                );
            }
        }

        record_error(close_reason.clone().into(), &self.send_txs_stats);

        // Determine next state based on close reason
        // Fatal errors transition to Closing, recoverable errors transition to Retry
        self.connection = match close_reason {
            ConnectionError::VersionMismatch | ConnectionError::LocallyClosed => {
                ConnectionState::Closing
            }
            _ => ConnectionState::Retry(0),
        };
    }

    /// Sends a batch of transactions using the provided `connection`.
    ///
    /// Each transaction in the batch is sent over the QUIC streams one at the
    /// time, which prevents traffic fragmentation and shows better TPS in
    /// comparison with multistream send. If the batch is determined to be
    /// outdated and flag `skip_check_transaction_age` is unset, it will be
    /// dropped without being sent.
    ///
    /// The method checks connection health before sending each transaction to
    /// avoid operations on a closed connection. In case of error, it doesn't
    /// retry to send the same transactions again but transitions to retry state.
    async fn send_transactions(&mut self, connection: Connection, transactions: TransactionBatch) {
        let now = timestamp();
        if !self.skip_check_transaction_age
            && now.saturating_sub(transactions.timestamp()) > MAX_PROCESSING_AGE_MS
        {
            debug!("Drop outdated transaction batch for peer: {}", self.peer);
            return;
        }

        let mut measure_send = Measure::start("send transaction batch");
        for data in transactions.into_iter() {
            // Check connection health before each send
            if connection.close_reason().is_some() {
                debug!("Connection closed during transaction batch sending");
                self.connection = ConnectionState::Retry(0);
                break;
            }

            let result = send_data_over_stream(&connection, &data).await;

            if let Err(error) = result {
                trace!(
                    "Failed to send transaction to {} over stream with error: {error}",
                    self.peer
                );
                record_error(error, &self.send_txs_stats);
                self.connection = ConnectionState::Retry(0);
                // Exit early since connection is likely broken
                break;
            } else {
                self.send_txs_stats
                    .successfully_sent
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        measure_send.stop();
        debug!(
            "Time to send transactions batch to {}: {} us",
            self.peer,
            measure_send.as_us()
        );
    }

    /// Attempts to create a new connection to the specified `peer` address.
    ///
    /// If the connection is successful, the state is updated to `Active`.
    ///
    /// If an error occurs, the state may transition to `Retry` or `Closing`,
    /// depending on the nature of the error.
    async fn create_connection(&mut self, retries_attempt: usize) {
        let server_name = socket_addr_to_quic_server_name(self.peer);
        let connecting = self.endpoint.connect(self.peer, &server_name);
        match connecting {
            Ok(connecting) => {
                let mut measure_connection = Measure::start("establish connection");
                let res = timeout(self.handshake_timeout, connecting).await;
                measure_connection.stop();
                debug!(
                    "Establishing connection with {} took: {} us",
                    self.peer,
                    measure_connection.as_us()
                );
                match res {
                    Ok(Ok(connection)) => {
                        self.connection = ConnectionState::Active(connection);
                    }
                    Ok(Err(err)) => {
                        warn!("Connection error {}: {}", self.peer, err);
                        record_error(err.into(), &self.send_txs_stats);
                        self.connection = ConnectionState::Retry(retries_attempt.saturating_add(1));
                    }
                    Err(_) => {
                        debug!(
                            "Connection to {} timed out after {:?}",
                            self.peer, self.handshake_timeout
                        );
                        record_error(QuicError::HandshakeTimeout, &self.send_txs_stats);
                        self.connection = ConnectionState::Retry(retries_attempt.saturating_add(1));
                    }
                }
            }
            Err(connecting_error) => {
                record_error(connecting_error.clone().into(), &self.send_txs_stats);
                match connecting_error {
                    ConnectError::EndpointStopping => {
                        debug!(
                            "Endpoint stopping, exit connection worker for peer: {}",
                            self.peer
                        );
                        self.connection = ConnectionState::Closing;
                    }
                    ConnectError::InvalidRemoteAddress(_) => {
                        warn!(
                            "Invalid remote address for peer: {}, attempt: {}",
                            self.peer, retries_attempt
                        );
                        self.connection = ConnectionState::Closing;
                    }
                    e => {
                        error!(
                            "Unexpected error has happened while trying to create connection to \
                             {}: {e}",
                            self.peer
                        );
                        self.connection = ConnectionState::Closing;
                    }
                }
            }
        }
    }

    /// Attempts to reconnect to the peer after a connection failure.
    async fn reconnect(&mut self, num_reconnects: usize) {
        debug!(
            "Trying to reconnect to {}. Reopen connection, 0rtt is not implemented yet.",
            self.peer
        );
        // We can reconnect using 0rtt, but not a priority for now. Check if we
        // need to call config.enable_0rtt() on the client side and where
        // session tickets are stored.
        self.create_connection(num_reconnects).await;
    }
}
