//! The `blockstore_metric_report_service` periodically reports ledger store metrics.

use {
    crate::blockstore::Blockstore,
    std::{
        string::ToString,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

// Determines how often we report blockstore metrics under
// BlockstoreMetricReportService. Note that there are other blockstore
// metrics that are reported outside BlockstoreMetricReportService.
const BLOCKSTORE_METRICS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

pub struct BlockstoreMetricReportService {
    t_cf_metric: JoinHandle<()>,
}

impl BlockstoreMetricReportService {
    pub fn new(blockstore: Arc<Blockstore>, exit: Arc<AtomicBool>) -> Self {
        let t_cf_metric = Builder::new()
            .name("solRocksCfMtrcs".to_string())
            .spawn(move || {
                info!("BlockstoreMetricReportService has started");
                let mut last_report_time = Instant::now();
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    if last_report_time.elapsed() > BLOCKSTORE_METRICS_REPORT_INTERVAL {
                        blockstore.submit_rocksdb_cf_metrics_for_all_cfs();

                        last_report_time = Instant::now();
                    }

                    thread::sleep(Duration::from_secs(1));
                }
                info!("BlockstoreMetricReportService has stopped");
            })
            .unwrap();
        Self { t_cf_metric }
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_cf_metric.join()
    }
}
