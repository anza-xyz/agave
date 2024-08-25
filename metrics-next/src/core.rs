use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue},
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::{exponential_buckets, Histogram},
    },
    registry::{Registry, Unit},
};

pub struct Metrics {
    pub optimistic_slot: Gauge,
}

impl Metrics {
    pub fn new_with_registry(registry: &mut Registry) -> Self {
        let optimistic_slot = Gauge::default();
        registry.register(
            "optimistic_slot",
            "optimistic_slot",
            optimistic_slot.clone(),
        );

        Self { optimistic_slot }
    }
}
