//! XDP worker pools, queue selections, and their validation.

mod runtime;

pub use runtime::XdpRuntimeConfig;
use {
    crate::{
        EffectiveConfig, NetworkDeviceSelector,
        interface::{NetworkInterface, format_interface_path, format_label},
    },
    serde::Deserialize,
    std::collections::BTreeSet,
};

// Bound the declared worker pool while allowing headroom above
// 256-channel NIC configurations. This limits worker count, not queue IDs.
pub(crate) const MAX_XDP_WORKERS: usize = 512;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum XdpWorkerPool {
    Auto { count: usize },
    Cpus(Vec<usize>),
    Bindings(Vec<XdpWorkerBinding>),
}

/// An XDP worker's hardware queue and the CPU it runs on.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct XdpWorkerBinding {
    pub queue: u32,
    pub cpu: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(
    rename_all = "snake_case",
    expecting = "\"all\" or an array of non-negative 32-bit queue IDs"
)]
pub(crate) enum QueueSelection {
    All,
    #[serde(untagged)]
    Explicit(Vec<u32>),
}

/// An interface's `xdp` section: zero-copy mode and its worker pool.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InterfaceXdp {
    pub(crate) zero_copy: bool,
    pub(crate) workers: XdpWorkerPool,
}

/// An endpoint's `[<endpoint>.xdp]` section.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointXdp {
    pub(crate) tx: EndpointTx,
}

/// The interface and hardware queues an endpoint transmits on.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointTx {
    pub(crate) interface: String,
    pub(crate) queues: QueueSelection,
}

/// The top-level `[xdp]` section.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigXdp {
    pub(crate) enabled: bool,
}

pub(crate) fn validate_single_interface(config: &EffectiveConfig) -> Result<(), String> {
    if config.interfaces.len() != 1 {
        return Err(format!(
            "XDP version 1 supports exactly one effective interface; found {}",
            config.interfaces.len()
        ));
    }
    Ok(())
}

pub(crate) fn validate_pool_len(len: usize, field: &str) -> Result<(), String> {
    if len == 0 || len > MAX_XDP_WORKERS {
        return Err(format!(
            "{field} must request between 1 and {MAX_XDP_WORKERS} workers; found {len}"
        ));
    }
    Ok(())
}

fn find_duplicate<T: Copy + Ord>(values: impl IntoIterator<Item = T>) -> Option<T> {
    let mut seen = BTreeSet::new();
    values.into_iter().find(|value| !seen.insert(*value))
}

pub(crate) fn validate_unique_cpus(cpus: &[usize], field: &str) -> Result<(), String> {
    if let Some(cpu) = find_duplicate(cpus.iter().copied()) {
        return Err(format!("{field} contains duplicate CPU {cpu}"));
    }
    Ok(())
}

impl XdpWorkerPool {
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self {
            Self::Auto { count } => validate_pool_len(*count, "workers.auto.count"),
            Self::Cpus(cpus) => {
                validate_pool_len(cpus.len(), "workers.cpus")?;
                validate_unique_cpus(cpus, "workers.cpus")
            }
            Self::Bindings(bindings) => {
                validate_pool_len(bindings.len(), "workers.bindings")?;
                if let Some(queue) = find_duplicate(bindings.iter().map(|binding| binding.queue)) {
                    return Err(format!("workers.bindings contains duplicate queue {queue}"));
                }
                if let Some(cpu) = find_duplicate(bindings.iter().map(|binding| binding.cpu)) {
                    return Err(format!("workers.bindings contains duplicate CPU {cpu}"));
                }
                Ok(())
            }
        }
    }
}

impl QueueSelection {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let Self::Explicit(queues) = self else {
            return Ok(());
        };
        if queues.is_empty() {
            return Err("tx.queues must not be an empty queue list".to_string());
        }
        if let Some(queue) = find_duplicate(queues.iter().copied()) {
            return Err(format!("tx.queues contains duplicate queue {queue}"));
        }
        Ok(())
    }
}

/// Queue ids an endpoint transmits over, in its own sender order.
fn select_endpoint_queue_ids(endpoint: &EndpointXdp, pool: &[u32]) -> Vec<u32> {
    match &endpoint.tx.queues {
        QueueSelection::All => pool.to_vec(),
        QueueSelection::Explicit(queues) => queues.clone(),
    }
}

fn collect_worker_queue_ids(workers: &XdpWorkerPool) -> Vec<u32> {
    match workers {
        XdpWorkerPool::Auto { count } => (0..*count as u32).collect(),
        XdpWorkerPool::Cpus(cpus) => (0..cpus.len() as u32).collect(),
        XdpWorkerPool::Bindings(bindings) => bindings.iter().map(|binding| binding.queue).collect(),
    }
}

/// One value per validator network endpoint that uses XDP. Generic over what each
/// endpoint holds: its config section (`EffectiveEndpoint`), its selected queue
/// ids ([`XdpPolicy`]), or its sender positions ([`XdpRuntimeConfig`]).
#[derive(Clone, Debug)]
pub struct XdpEndpoints<T> {
    pub gossip: T,
    pub repair: T,
    pub tpu: T,
    pub turbine: T,
    pub votor: T,
}

impl<T> XdpEndpoints<T> {
    fn map<U>(&self, mut f: impl FnMut(&T) -> U) -> XdpEndpoints<U> {
        XdpEndpoints {
            gossip: f(&self.gossip),
            repair: f(&self.repair),
            tpu: f(&self.tpu),
            turbine: f(&self.turbine),
            votor: f(&self.votor),
        }
    }

    fn named(&self) -> [(&'static str, &T); 5] {
        let Self {
            gossip,
            repair,
            tpu,
            turbine,
            votor,
        } = self;
        [
            ("gossip", gossip),
            ("repair", repair),
            ("tpu", tpu),
            ("turbine", turbine),
            ("votor", votor),
        ]
    }

    pub(crate) fn get_values(&self) -> [&T; 5] {
        self.named().map(|(_, value)| value)
    }
}

/// The active XDP policy for the single transmit interface.
#[derive(Clone, Debug)]
pub struct XdpPolicy {
    pub(crate) interface_label: String,
    pub(crate) interface: NetworkInterface,
    /// Queue ids each endpoint transmits over, in its own sender order.
    pub(crate) queues: XdpEndpoints<Vec<u32>>,
}

/// Validate schema constraints independently of host state, even when XDP is
/// disabled. Deserialization alone does not check them.
pub(crate) fn validate_structural(config: &EffectiveConfig) -> Result<(), String> {
    for (label, interface) in &config.interfaces {
        interface
            .xdp
            .workers
            .validate()
            .map_err(|error| format!("{}.xdp.{error}", format_interface_path(label)))?;
        if matches!(interface.xdp.workers, XdpWorkerPool::Bindings(_))
            && !matches!(interface.device, NetworkDeviceSelector::Name(_))
        {
            return Err(format!(
                "{}.xdp.workers.bindings requires device.name; use workers.auto/workers.cpus with \
                 device.route, or name the device",
                format_interface_path(label)
            ));
        }
    }
    for (name, endpoint) in endpoint_xdp(config).named() {
        endpoint
            .tx
            .queues
            .validate()
            .map_err(|error| format!("{name}.xdp.{error}"))?;
    }
    Ok(())
}

fn endpoint_xdp(config: &EffectiveConfig) -> XdpEndpoints<&EndpointXdp> {
    config.endpoints().map(|&endpoint| &endpoint.xdp)
}

/// Validate schema constraints and host-independent cross-references, returning
/// the XDP policy when XDP is enabled.
pub(crate) fn validate(
    config: &EffectiveConfig,
) -> Result<(Option<XdpPolicy>, Vec<String>), String> {
    validate_structural(config)?;
    for (name, endpoint) in endpoint_xdp(config).named() {
        let label = &endpoint.tx.interface;
        let interface = config.interfaces.get(label).ok_or_else(|| {
            format!(
                "{name}.xdp.tx.interface names {}, which is not a declared interface; declared: \
                 [{}]",
                format_label(label),
                config
                    .interfaces
                    .keys()
                    .map(|label| format_label(label))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if let QueueSelection::Explicit(queues) = &endpoint.tx.queues {
            let pool: BTreeSet<_> = collect_worker_queue_ids(&interface.xdp.workers)
                .into_iter()
                .collect();
            let missing: Vec<_> = queues
                .iter()
                .filter(|queue| !pool.contains(queue))
                .map(ToString::to_string)
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "{name}.xdp.tx.queues references queue(s) {} not declared by {}.xdp.workers",
                    missing.join(", "),
                    format_interface_path(label)
                ));
            }
        }
    }
    if !config.xdp.enabled {
        return Ok((None, Vec::new()));
    }
    validate_single_interface(config)?;
    let (interface_label, interface) = config
        .interfaces
        .first_key_value()
        .expect("XDP config should contain exactly one interface after validation");
    let pool = collect_worker_queue_ids(&interface.xdp.workers);
    let queues = endpoint_xdp(config).map(|endpoint| select_endpoint_queue_ids(endpoint, &pool));
    let selections = queues.get_values();
    let warnings = pool
        .iter()
        .filter(|queue| !selections.iter().any(|selected| selected.contains(queue)))
        .map(|queue| {
            format!(
                "worker queue {queue} on interface {} is inactive (unreferenced)",
                format_label(interface_label)
            )
        })
        .collect();
    let xdp = XdpPolicy {
        interface_label: interface_label.clone(),
        interface: interface.clone(),
        queues,
    };
    Ok((Some(xdp), warnings))
}
