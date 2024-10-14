use {
    crossbeam_channel::Receiver,
    log::error,
    solana_metrics::datapoint_info,
    solana_sdk::signature::Signature,
    solana_tps_client::TpsClient,
    solana_transaction_status::TransactionConfirmationStatus,
    std::{
        sync::Arc,
        thread::{sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

pub(crate) enum HttpOrWs {
    Http,
    Ws,
}

pub(crate) fn measure_tx_latency<T: TpsClient + Send + Sync + ?Sized + 'static>(
    receiver: Receiver<(Signature, Instant)>,
    http_or_ws: HttpOrWs,
    http_sleep_ms: u64,
    client: Arc<T>,
) -> JoinHandle<()> {
    Builder::new()
        .name("measureTxLatency".to_string())
        .spawn(move || loop {
            let (signature, sent_at) = match receiver.recv() {
                Ok(s) => s,
                Err(_) => break,
            };
            match http_or_ws {
                HttpOrWs::Http => {
                    let mut current_status = None;

                    loop {
                        let mut statuses = match client.get_signature_statuses(&[signature]) {
                            Ok(statuses) => statuses,
                            Err(e) => {
                                error!("Failed to get status of signature {signature}: {e}");
                                sleep(Duration::from_millis(http_sleep_ms));
                                continue;
                            }
                        };

                        match (
                            current_status,
                            statuses.remove(0).map(|status| status.confirmation_status),
                        ) {
                            (None, Some(Some(TransactionConfirmationStatus::Confirmed))) => {
                                current_status = Some(TransactionConfirmationStatus::Confirmed);
                                datapoint_info!(
                                    "ext-tx-latency-confirmed",
                                    ("signature", signature.to_string(), String),
                                    ("latency", sent_at.elapsed().as_millis(), i64)
                                );
                            }
                            (_, Some(Some(TransactionConfirmationStatus::Finalized))) => {
                                current_status = Some(TransactionConfirmationStatus::Finalized);
                                datapoint_info!(
                                    "ext-tx-latency-finalized",
                                    ("signature", signature.to_string(), String),
                                    ("latency", sent_at.elapsed().as_millis(), i64)
                                );
                                break;
                            }
                            (_, _) => {
                                // do nothing here
                            }
                        }
                        sleep(Duration::from_millis(http_sleep_ms));
                    }
                }
                HttpOrWs::Ws => {}
            }
            println!("Got transaction signature: {}", signature);
        })
        .expect("measureTxLatency should have started successfully.")
}
