//! If `client-key-updater` feature is activated, this module provides
//! [`ClientKeyUpdater`] structure to simplify identity key update in validator
//! code.
use {
    crate::{
        connection_workers_scheduler::ConnectionWorkersSchedulerConfig,
        leader_updater::LeaderUpdater, transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler, ConnectionWorkersSchedulerError,
    },
    log::{debug, error},
    std::{
        sync::{Arc, Mutex},
        time::Duration,
    },
    tokio::{
        runtime::Handle,
        sync::mpsc::{self},
        task::JoinHandle as TokioJoinHandle,
    },
    tokio_util::sync::CancellationToken,
};

const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(3);

/// [`ClientKeyUpdater`] is a helper structure that encapsulates common logic
/// for implementing a client that supports updating its identity key.
///
/// This functionality is only relevant for client implementations that run
/// inside the validator to simplify implementation of `NotifyKeyUpdate`.
///
/// It provides:
/// * Initialization of the scheduler with runtime configuration.
/// * Updating the validator's identity keypair and propagating the change to
///   the scheduler.
#[derive(Clone)]
pub struct ClientKeyUpdater {
    pub runtime_handle: Handle,
    // This handle is needed to implement `NotifyKeyUpdate` trait. It's only
    // method takes &self and thus we need to wrap with Mutex.
    pub join_and_cancel: Arc<Mutex<(Option<TpuClientJoinHandle>, CancellationToken)>>,
}

type TpuClientJoinHandle =
    TokioJoinHandle<Result<ConnectionWorkersScheduler, ConnectionWorkersSchedulerError>>;

impl ClientKeyUpdater {
    pub fn new<L>(
        runtime_handle: Handle,
        transaction_receiver: mpsc::Receiver<TransactionBatch>,
        leader_updater: L,
        config: ConnectionWorkersSchedulerConfig,
        metrics_name: &'static str,
    ) -> Self
    where
        L: LeaderUpdater + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let scheduler =
            ConnectionWorkersScheduler::new(Box::new(leader_updater), transaction_receiver);
        // leaking handle to this task, as it will run until the cancel signal is received
        runtime_handle.spawn(scheduler.get_stats().report_to_influxdb(
            metrics_name,
            METRICS_REPORTING_INTERVAL,
            cancel.clone(),
        ));
        let handle = runtime_handle.spawn(scheduler.run(config, cancel.clone()));
        Self {
            runtime_handle,
            join_and_cancel: Arc::new(Mutex::new((Some(handle), cancel))),
        }
    }

    pub async fn do_update_key(
        &self,
        config: ConnectionWorkersSchedulerConfig,
        metrics_name: &'static str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let runtime_handle = self.runtime_handle.clone();
        let handle = self.join_and_cancel.clone();

        let join_handle = {
            let Ok(mut lock) = handle.lock() else {
                return Err("TpuClientNext task panicked.".into());
            };
            let (handle, token) = std::mem::take(&mut *lock);
            token.cancel();
            handle
        };

        if let Some(join_handle) = join_handle {
            let Ok(result) = join_handle.await else {
                return Err("TpuClientNext task panicked.".into());
            };

            match result {
                Ok(scheduler) => {
                    let cancel = CancellationToken::new();
                    // leaking handle to this task, as it will run until the cancel signal is received
                    runtime_handle.spawn(scheduler.get_stats().report_to_influxdb(
                        metrics_name,
                        METRICS_REPORTING_INTERVAL,
                        cancel.clone(),
                    ));
                    let join_handle = runtime_handle.spawn(scheduler.run(config, cancel.clone()));

                    let Ok(mut lock) = handle.lock() else {
                        return Err("TpuClientNext task panicked.".into());
                    };
                    *lock = (Some(join_handle), cancel);
                }
                Err(error) => {
                    return Err(Box::new(error));
                }
            }
        }
        Ok(())
    }

    pub fn exit(&self) {
        let Ok(mut lock) = self.join_and_cancel.lock() else {
            error!("Failed to stop scheduler: TpuClientNext task panicked.");
            return;
        };
        let (cancel, token) = std::mem::take(&mut *lock);
        token.cancel();
        let Some(handle) = cancel else {
            error!("Client task handle was not set.");
            return;
        };
        match self.runtime_handle.block_on(handle) {
            Ok(result) => match result {
                Ok(scheduler) => {
                    debug!(
                        "tpu-client-next statistics over all the connections: {:?}",
                        scheduler.get_stats()
                    );
                }
                Err(error) => error!("tpu-client-next exits with error {error}."),
            },
            Err(error) => error!("Failed to join task {error}."),
        }
    }
}
