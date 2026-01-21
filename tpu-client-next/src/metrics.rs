//! If `metrics` feature is activated, this module provides `report_to_influxdb`
//! method for [`SendTransactionStats`] which periodically reports transaction
//! sending statistics to InfluxDB.
use {
    crate::SendTransactionStats,
    solana_metrics::datapoint_info,
    std::{sync::Arc, time::Duration},
    tokio::{select, time::interval},
    tokio_util::sync::CancellationToken,
};

impl SendTransactionStats {
    /// Report the statistics to influxdb in a compact form.
    #[allow(clippy::arithmetic_side_effects)]
    pub async fn report_to_influxdb(
        self: Arc<Self>,
        name: &'static str,
        reporting_interval: Duration,
        cancel: CancellationToken,
    ) {
        let mut interval = interval(reporting_interval);
        let stats = self.clone();
        loop {
            select! {
                    _ = interval.tick() => {
                    let view = stats.read_and_reset();
                    let connect_error = view.connect_error_cids_exhausted
                        + view.connect_error_other
                        + view.connect_error_invalid_remote_address;
                    let connection_error = view.connection_error_reset
                        + view.connection_error_cids_exhausted
                        + view.connection_error_timed_out
                        + view.connection_error_application_closed
                        + view.connection_error_transport_error
                        + view.connection_error_version_mismatch
                        + view.connection_error_locally_closed;
                    let write_error = view.write_error_stopped
                        + view.write_error_closed_stream
                        + view.write_error_connection_lost
                        + view.write_error_zero_rtt_rejected;

                    datapoint_info!(
                        name,
                        ("connect_error", connect_error, i64),
                        ("connection_error", connection_error, i64),
                        ("successfully_sent", view.successfully_sent, i64),
                        ("congestion_events", view.transport_congestion_events, i64),
                        ("write_error", write_error, i64),
                    );
                }
                _ = cancel.cancelled() => break,
            }
        }
    }
}
