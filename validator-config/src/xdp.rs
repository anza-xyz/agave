//! XDP worker and queue policies, validation, and CPU assignment.

use {
    crate::{Components, DeviceSelector, EffectiveConfig, Source, interface::interface_path},
    serde::Deserialize,
    std::collections::{BTreeMap, BTreeSet},
};

pub(crate) const MAX_XDP_WORKERS: usize = 4096;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WorkerPolicy {
    Auto { count: usize },
    Cpus(Vec<usize>),
    Bindings(Vec<QueueCpuBinding>),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueueCpuBinding {
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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InterfaceXdp {
    pub(crate) zero_copy: bool,
    pub(crate) workers: WorkerPolicy,
    #[serde(skip)]
    pub(crate) zero_copy_source: Source,
    #[serde(skip)]
    pub(crate) workers_source: Source,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentXdp {
    pub(crate) tx: ComponentTx,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentTx {
    pub(crate) interface: String,
    pub(crate) queues: QueueSelection,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobalXdp {
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug)]
pub struct RuntimeXdpConfig {
    pub interface_label: String,
    pub device: DeviceSelector,
    pub queues: Vec<QueueCpuBinding>,
    pub zero_copy: bool,
    pub components: Components<Box<[usize]>>,
}

pub(crate) fn validate_pool_len(len: usize, field: &str) -> Result<(), String> {
    if len == 0 || len > MAX_XDP_WORKERS {
        return Err(format!(
            "{field} must contain between 1 and {MAX_XDP_WORKERS} workers; found {len}"
        ));
    }
    Ok(())
}

pub(crate) fn validate_unique_cpus(cpus: &[usize], field: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    if let Some(cpu) = cpus.iter().find(|cpu| !seen.insert(**cpu)) {
        return Err(format!("{field} contains duplicate CPU {cpu}"));
    }
    Ok(())
}

impl WorkerPolicy {
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self {
            Self::Auto { count } => validate_pool_len(*count, "workers.auto.count"),
            Self::Cpus(cpus) => {
                validate_pool_len(cpus.len(), "workers.cpus")?;
                validate_unique_cpus(cpus, "workers.cpus")
            }
            Self::Bindings(bindings) => {
                validate_pool_len(bindings.len(), "workers.bindings")?;
                let mut queues = BTreeSet::new();
                let mut cpus = BTreeSet::new();
                for QueueCpuBinding { queue, cpu } in bindings {
                    if !queues.insert(queue) {
                        return Err(format!("workers.bindings contains duplicate queue {queue}"));
                    }
                    if !cpus.insert(cpu) {
                        return Err(format!("workers.bindings contains duplicate CPU {cpu}"));
                    }
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
        if queues.len() > MAX_XDP_WORKERS {
            return Err(format!(
                "tx.queues exceeds MAX_XDP_WORKERS ({MAX_XDP_WORKERS})"
            ));
        }
        let mut seen = BTreeSet::new();
        for queue in queues {
            if !seen.insert(*queue) {
                return Err(format!("tx.queues contains duplicate queue {queue}"));
            }
        }
        Ok(())
    }
}

/// Queue ids a component transmits over, in its own sender order.
fn component_queue_ids<'a>(component: &'a ComponentXdp, pool: &'a [u32]) -> &'a [u32] {
    match &component.tx.queues {
        QueueSelection::All => pool,
        QueueSelection::Explicit(queues) => queues,
    }
}

fn worker_queue_ids(policy: &WorkerPolicy) -> Vec<u32> {
    match policy {
        WorkerPolicy::Auto { count } => (0..*count as u32).collect(),
        WorkerPolicy::Cpus(cpus) => (0..cpus.len() as u32).collect(),
        WorkerPolicy::Bindings(bindings) => bindings.iter().map(|binding| binding.queue).collect(),
    }
}

/// Validate schema constraints and host-independent cross-references. Invalid
/// cross-references are fatal when XDP is enabled and warnings when disabled.
pub fn validate_policy(config: &EffectiveConfig) -> Result<Vec<String>, String> {
    config.validate_structural()?;
    let mut warnings = Vec::new();
    let active = config.xdp_active();
    if active && config.interfaces.len() != 1 {
        return Err(format!(
            "XDP version 1 supports exactly one effective interface; found {}",
            config.interfaces.len()
        ));
    }
    for (name, component) in config.named_components() {
        let label = &component.tx.interface;
        let Some(interface) = config.interfaces.get(label) else {
            let message = format!(
                "{name}.xdp.tx.interface names {label:?}, which is not a declared interface; \
                 declared: {:?}",
                config.interfaces.keys().collect::<Vec<_>>()
            );
            if active {
                return Err(message);
            }
            warnings.push(message);
            continue;
        };
        if let QueueSelection::Explicit(queues) = &component.tx.queues {
            let pool: BTreeSet<_> = worker_queue_ids(&interface.xdp.workers)
                .into_iter()
                .collect();
            let missing: Vec<_> = queues
                .iter()
                .filter(|queue| !pool.contains(queue))
                .collect();
            if !missing.is_empty() {
                let message = format!(
                    "{name}.xdp.tx.queues references queue(s) {} not declared by {}.xdp.workers",
                    missing
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    interface_path(label)
                );
                if active {
                    return Err(message);
                }
                warnings.push(message);
            }
        }
    }
    if !active {
        return Ok(warnings);
    }
    let (label, interface) = config
        .interfaces
        .iter()
        .next()
        .expect("XDP config should contain exactly one interface after validation");
    let pool = worker_queue_ids(&interface.xdp.workers);
    let selections = config
        .named_components()
        .map(|(_, component)| component_queue_ids(component, &pool));
    for queue in &pool {
        if selections.iter().any(|queues| queues.contains(queue)) {
            continue;
        }
        let message = format!(
            "{} worker queue {queue} on interface {label:?} is inactive (unreferenced)",
            match interface.xdp.workers_source {
                Source::BuiltIn => "built-in",
                Source::User => "user-authored",
                Source::Cli => "CLI-authored",
            }
        );
        warnings.push(message);
    }
    Ok(warnings)
}

fn resolve_declared_workers(
    policy: &WorkerPolicy,
    allowed_cpus: &BTreeSet<usize>,
    poh_core: Option<usize>,
) -> Result<Vec<QueueCpuBinding>, String> {
    let verify_cpu = |cpu: usize| -> Result<(), String> {
        if !allowed_cpus.contains(&cpu) {
            return Err(format!(
                "XDP worker CPU {cpu} is not in the process CPU-affinity set"
            ));
        }
        if Some(cpu) == poh_core {
            return Err(format!("XDP worker CPU {cpu} overlaps the PoH core"));
        }
        Ok(())
    };
    // Each mode contributes only the CPU order; worker_queue_ids owns the queue-id
    // rule so validation and resolution cannot disagree about the declared pool.
    let cpus: Vec<usize> = match policy {
        WorkerPolicy::Auto { count } => {
            let eligible: Vec<_> = allowed_cpus
                .iter()
                .rev()
                .copied()
                .filter(|cpu| Some(*cpu) != poh_core)
                .take(*count)
                .collect();
            if eligible.len() < *count {
                return Err(format!(
                    "workers.auto.count = {count} requires {count} eligible CPUs, but only {} \
                     remain after excluding PoH",
                    eligible.len()
                ));
            }
            eligible
        }
        WorkerPolicy::Cpus(cpus) => {
            for cpu in cpus {
                verify_cpu(*cpu)?;
            }
            cpus.clone()
        }
        WorkerPolicy::Bindings(bindings) => {
            for binding in bindings {
                verify_cpu(binding.cpu)?;
            }
            bindings.iter().map(|binding| binding.cpu).collect()
        }
    };
    Ok(worker_queue_ids(policy)
        .into_iter()
        .zip(cpus)
        .map(|(queue, cpu)| QueueCpuBinding { queue, cpu })
        .collect())
}

pub fn resolve_runtime(
    config: &EffectiveConfig,
    allowed_cpus: &BTreeSet<usize>,
    poh_core: Option<usize>,
) -> Result<(RuntimeXdpConfig, Vec<String>), String> {
    let warnings = validate_policy(config)?;
    if !config.xdp_active() {
        return Err("cannot resolve an inactive XDP policy".to_string());
    }
    let (label, interface) = config
        .interfaces
        .iter()
        .next()
        .expect("XDP config should contain exactly one interface after validation");
    let declared = resolve_declared_workers(&interface.xdp.workers, allowed_cpus, poh_core)?;
    let pool: Vec<u32> = declared.iter().map(|binding| binding.queue).collect();
    let selected = Components {
        gossip: component_queue_ids(&config.gossip.xdp, &pool),
        repair: component_queue_ids(&config.repair.xdp, &pool),
        tpu: component_queue_ids(&config.tpu.xdp, &pool),
        turbine: component_queue_ids(&config.turbine.xdp, &pool),
        votor: component_queue_ids(&config.votor.xdp, &pool),
    };
    let active_ids: BTreeSet<_> = selected
        .values()
        .into_iter()
        .flat_map(|queues| queues.iter())
        .copied()
        .collect();
    let active_workers: Vec<_> = declared
        .into_iter()
        .filter(|binding| active_ids.contains(&binding.queue))
        .collect();
    // Worker CPUs are unique and belong to the allowed set.
    if active_workers.len() == allowed_cpus.len() {
        return Err(
            "XDP workers must leave at least one process CPU unreserved for the main thread"
                .to_string(),
        );
    }
    let positions: BTreeMap<_, _> = active_workers
        .iter()
        .enumerate()
        .map(|(position, binding)| (binding.queue, position))
        .collect();
    let component_positions = |queues: &[u32]| -> Box<[usize]> {
        queues
            .iter()
            .map(|queue| {
                *positions
                    .get(queue)
                    .expect("validated selected queue must have an active worker")
            })
            .collect()
    };
    let components = Components {
        gossip: component_positions(selected.gossip),
        repair: component_positions(selected.repair),
        tpu: component_positions(selected.tpu),
        turbine: component_positions(selected.turbine),
        votor: component_positions(selected.votor),
    };
    Ok((
        RuntimeXdpConfig {
            interface_label: label.clone(),
            device: interface.device.clone(),
            queues: active_workers,
            zero_copy: interface.xdp.zero_copy,
            components,
        },
        warnings,
    ))
}
