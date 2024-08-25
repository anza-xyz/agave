use {
    crate::metrics::Metrics,
    std::sync::{Arc, OnceLock},
};

static METRICS: OnceLock<Option<Arc<Metrics>>> = OnceLock::new();

pub fn initialize_metrics(metrics: Option<Arc<Metrics>>) -> Result<(), Option<Arc<Metrics>>> {
    METRICS.set(metrics)
}

pub fn get_metrics() -> Option<Arc<Metrics>> {
    METRICS.get().and_then(|opt| opt.clone())
}
