use {
    crossbeam_channel::{Receiver, RecvTimeoutError},
    itertools::izip,
    solana_accounts_db::ancestors::Ancestors,
    solana_clock::Slot,
    solana_ledger::blockstore_processor::TransactionStatusMessage,
    solana_runtime::status_cache::StatusCache,
    solana_signature::Signature,
    solana_transaction_error::TransactionResult,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::Builder,
        time::Duration,
    },
};

type RecentTransactionResultCache = StatusCache<TransactionResult<()>>;

#[derive(Default)]
pub struct RecentTransactionStatusService {
    transaction_result_cache: Arc<RwLock<RecentTransactionResultCache>>,
}

impl RecentTransactionStatusService {
    const SERVICE_NAME: &str = "RecentTransactionStatusService";

    pub fn new(
        transaction_result_cache: RecentTransactionResultCache,
        transaction_status_receiver: Receiver<TransactionStatusMessage>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let transaction_result_cache = Arc::new(RwLock::new(transaction_result_cache));

        Builder::new()
            .name("solRecentTxStatus".to_string())
            .spawn({
                let arc_transaction_result_cache = transaction_result_cache.clone();
                let arc_transaction_status_receiver = Arc::new(transaction_status_receiver.clone());
                move || {
                    info!("{} has started", Self::SERVICE_NAME);
                    loop {
                        if exit.load(Ordering::Relaxed) {
                            break;
                        }

                        let message = match arc_transaction_status_receiver
                            .recv_timeout(Duration::from_secs(1))
                        {
                            Ok(message) => message,
                            Err(err @ RecvTimeoutError::Disconnected) => {
                                info!("{} is stopping because: {err}", Self::SERVICE_NAME);
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                continue;
                            }
                        };

                        fn handle_message(
                            transaction_result_cache: &Arc<RwLock<RecentTransactionResultCache>>,
                            message: TransactionStatusMessage,
                        ) -> Result<(), Box<dyn std::error::Error + '_>> {
                            match message {
                                TransactionStatusMessage::Batch(batch) => {
                                    let mut status_cache = transaction_result_cache.write()?;
                                    for (commit_result, transaction) in
                                        izip!(batch.commit_results, batch.transactions)
                                    {
                                        status_cache.insert(
                                            transaction.message().recent_blockhash(),
                                            transaction.signature(),
                                            batch.slot,
                                            commit_result.map(|_| ()),
                                        );
                                    }
                                    Ok(())
                                }
                                _ => Ok(()),
                            }
                        }

                        if let Err(err) = handle_message(&arc_transaction_result_cache, message) {
                            error!("{} is stopping because: {err}", Self::SERVICE_NAME);
                            exit.store(true, Ordering::Relaxed);
                            break;
                        };
                    }
                    info!("{} has stopped", Self::SERVICE_NAME);
                }
            })
            .unwrap();
        Self {
            transaction_result_cache,
        }
    }

    pub fn get_transaction_status(
        &self,
        signature: &Signature,
        ancestors: &Ancestors,
    ) -> Option<(Slot, TransactionResult<()>)> {
        let rcache = self
            .transaction_result_cache
            .read()
            .expect("Failed to obtain shared read access to status cache");
        rcache.get_status_any_blockhash(signature, ancestors)
    }
}
