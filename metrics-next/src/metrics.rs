use {
    crate::core,
    prometheus_client::{encoding::text::encode, registry::Registry},
};

pub struct Metrics {
    pub registry: Registry,
    pub core: core::Metrics,
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("agave");

        let sub_registry = registry.sub_registry_with_prefix("core");
        let core = core::Metrics::new_with_registry(sub_registry);

        // more metrics ...

        Self { registry, core }
    }

    // TODO: should rename to something meaningful
    pub fn dump(&self) -> Result<String, std::fmt::Error> {
        let mut buffer = String::new();
        encode(&mut buffer, &self.registry)?;
        Ok(buffer)
    }
}
