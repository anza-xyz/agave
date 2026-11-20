//! XDP worker pools, queue selections, and their validation.

mod runtime;

pub use runtime::XdpRuntimeConfig;
use {
    crate::{
        Diagnostic, DiagnosticKind, EffectiveConfig, NetworkDeviceSelector,
        interface::{NetworkInterface, format_interface_path, format_label},
    },
    serde::Deserialize,
    std::collections::BTreeSet,
};

// Bound the declared worker pool while allowing headroom above
// 256-channel NIC configurations. This limits worker count, not queue IDs.
pub(crate) const MAX_XDP_WORKERS: usize = 512;

/// How the XDP worker pool assigns hardware queues to CPUs.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum XdpWorkerPool {
    /// Assign CPUs automatically at runtime.
    Auto { count: usize },
    /// Assign queue IDs in sequence using these CPUs in order.
    Cpus(Vec<usize>),
    /// Assign each worker an explicit queue ID and CPU.
    Bindings(Vec<XdpWorkerBinding>),
}

/// An XDP worker's hardware queue and the CPU it runs on.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct XdpWorkerBinding {
    pub queue: u32,
    pub cpu: usize,
}

/// The queue selection for an endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(
    rename_all = "snake_case",
    expecting = "\"all\" or an array of non-negative 32-bit queue IDs"
)]
pub enum QueueSelection {
    /// Every declared worker queue.
    All,
    /// Specific hardware queue IDs, in sender order.
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
pub struct EndpointTx {
    pub(crate) interface: String,
    pub(crate) queues: QueueSelection,
}

impl EndpointTx {
    /// The logical interface label.
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// The effective selector for this endpoint's hardware queues.
    pub fn queues(&self) -> &QueueSelection {
        &self.queues
    }
}

/// The top-level `[xdp]` section.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigXdp {
    pub(crate) enabled: bool,
}

fn validate_single_interface(config: &EffectiveConfig) -> Result<(), Diagnostic> {
    if config.interfaces.len() != 1 {
        return Err(Diagnostic::new(
            DiagnosticKind::InvalidValue,
            format!(
                "must contain exactly one effective interface when XDP is enabled; found {}",
                config.interfaces.len()
            ),
        )
        .at_field(["interfaces"]));
    }
    Ok(())
}

fn validate_pool_len(len: usize) -> Result<(), String> {
    if len == 0 || len > MAX_XDP_WORKERS {
        return Err(format!(
            "must request between 1 and {MAX_XDP_WORKERS} workers; found {len}"
        ));
    }
    Ok(())
}

fn find_duplicate<T: Copy + Ord>(values: impl IntoIterator<Item = T>) -> Option<T> {
    let mut seen = BTreeSet::new();
    values.into_iter().find(|value| !seen.insert(*value))
}

impl XdpWorkerPool {
    fn field_path(&self) -> &'static [&'static str] {
        match self {
            Self::Auto { .. } => &["workers", "auto", "count"],
            Self::Cpus(_) => &["workers", "cpus"],
            Self::Bindings(_) => &["workers", "bindings"],
        }
    }

    pub(crate) fn validate(&self) -> Result<(), Diagnostic> {
        let invalid = |message| {
            Diagnostic::new(DiagnosticKind::InvalidValue, message)
                .at_field(self.field_path().iter().copied())
        };
        match self {
            Self::Auto { count } => validate_pool_len(*count).map_err(invalid),
            Self::Cpus(cpus) => {
                validate_pool_len(cpus.len()).map_err(invalid)?;
                if let Some(cpu) = find_duplicate(cpus.iter().copied()) {
                    return Err(invalid(format!("contains duplicate CPU {cpu}")));
                }
                Ok(())
            }
            Self::Bindings(bindings) => {
                validate_pool_len(bindings.len()).map_err(invalid)?;
                if let Some(queue) = find_duplicate(bindings.iter().map(|binding| binding.queue)) {
                    return Err(invalid(format!("contains duplicate queue {queue}")));
                }
                if let Some(cpu) = find_duplicate(bindings.iter().map(|binding| binding.cpu)) {
                    return Err(invalid(format!("contains duplicate CPU {cpu}")));
                }
                Ok(())
            }
        }
    }
}

impl QueueSelection {
    fn validate(&self) -> Result<(), Diagnostic> {
        let Self::Explicit(queues) = self else {
            return Ok(());
        };
        let invalid = |message| {
            Diagnostic::new(DiagnosticKind::InvalidValue, message).at_field(["tx", "queues"])
        };
        if queues.is_empty() {
            return Err(invalid("must not be an empty queue list".to_owned()));
        }
        if let Some(queue) = find_duplicate(queues.iter().copied()) {
            return Err(invalid(format!("contains duplicate queue {queue}")));
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
/// endpoint holds: its transmit settings ([`EndpointTx`]), its selected queue
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
    pub(crate) fn map<'a, U>(&'a self, mut f: impl FnMut(&'a T) -> U) -> XdpEndpoints<U> {
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

impl XdpPolicy {
    /// The logical label of the active transmit interface.
    pub fn interface_label(&self) -> &str {
        &self.interface_label
    }

    /// The active interface's device and worker settings.
    pub fn interface(&self) -> &NetworkInterface {
        &self.interface
    }

    /// The selected hardware queue IDs for each endpoint, in sender order.
    pub fn queues(&self) -> XdpEndpoints<&[u32]> {
        self.queues.map(Vec::as_slice)
    }
}

/// Validate schema constraints independently of host state, even when XDP is
/// disabled. Deserialization alone does not check them.
fn validate_structural(config: &EffectiveConfig) -> Result<(), Diagnostic> {
    for (label, interface) in &config.interfaces {
        interface
            .device
            .validate()
            .map_err(|error| error.at_field(["interfaces", label]))?;
        interface
            .xdp
            .workers
            .validate()
            .map_err(|error| error.at_field(["interfaces", label, "xdp"]))?;
        if matches!(interface.xdp.workers, XdpWorkerPool::Bindings(_))
            && !matches!(interface.device, NetworkDeviceSelector::Name(_))
        {
            return Err(Diagnostic::new(
                DiagnosticKind::InvalidValue,
                "requires device.name; use workers.auto/workers.cpus with device.route, or name \
                 the device",
            )
            .at_field(["interfaces", label, "xdp", "workers", "bindings"]));
        }
    }
    for (name, endpoint) in config.endpoints().named() {
        endpoint
            .tx
            .queues
            .validate()
            .map_err(|error| error.at_field([name, "xdp"]))?;
    }
    Ok(())
}

/// Validate schema constraints and host-independent cross-references, returning
/// the XDP policy when XDP is enabled.
pub(crate) fn validate(
    config: &EffectiveConfig,
) -> Result<(Option<XdpPolicy>, Vec<Diagnostic>), Diagnostic> {
    validate_structural(config)?;
    for (name, endpoint) in config.endpoints().named() {
        let label = &endpoint.tx.interface;
        let interface = config.interfaces.get(label).ok_or_else(|| {
            Diagnostic::new(
                DiagnosticKind::InvalidReference,
                format!(
                    "names {}, which is not a declared interface; declared: [{}]",
                    format_label(label),
                    config
                        .interfaces
                        .keys()
                        .map(|label| format_label(label))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .at_field([name, "xdp", "tx", "interface"])
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
                return Err(Diagnostic::new(
                    DiagnosticKind::InvalidReference,
                    format!(
                        "references queue(s) {} not declared by {}.xdp.workers",
                        missing.join(", "),
                        format_interface_path(label)
                    ),
                )
                .at_field([name, "xdp", "tx", "queues"]));
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
    let queues = config
        .endpoints()
        .map(|endpoint| select_endpoint_queue_ids(endpoint, &pool));
    let selections = queues.get_values();
    let warnings = pool
        .iter()
        .filter(|queue| !selections.iter().any(|selected| selected.contains(queue)))
        .map(|queue| {
            Diagnostic::new(
                DiagnosticKind::InactiveXdpQueue,
                format!(
                    "worker queue {queue} on interface {} is inactive (unreferenced)",
                    format_label(interface_label)
                ),
            )
            .at_field(interface.xdp.workers.field_path().iter().copied())
            .at_field(["interfaces", interface_label, "xdp"])
        })
        .collect();
    let xdp = XdpPolicy {
        interface_label: interface_label.clone(),
        interface: interface.clone(),
        queues,
    };
    Ok((Some(xdp), warnings))
}
