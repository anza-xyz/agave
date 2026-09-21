//! Embedded and user TOML are merged before being deserialized into one policy
//! model. CLI overrides retain the provenance needed for diagnostics, while
//! host-dependent worker and device resolution happens only for active XDP.

use {
    agave_xdp::transmitter::QueueCpuBinding,
    serde::Deserialize,
    std::{
        collections::{BTreeMap, BTreeSet},
        path::Path,
    },
};

const SCHEMA_VERSION: u32 = 1;
const MAX_XDP_WORKERS: usize = 4096;

/// The embedded default policy, as shipped, for `--print-default-config`.
pub(crate) const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Source {
    #[default]
    BuiltIn,
    User,
    Cli,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DeviceSelector {
    DefaultRoute,
    Name(String),
}

#[derive(Clone, Debug, PartialEq)]
enum WorkerPolicy {
    Auto { count: usize },
    Cpus(Vec<usize>),
    Bindings(Vec<QueueCpuBinding>),
}

#[derive(Clone, Debug)]
enum QueueSelection {
    All,
    Explicit(Vec<u32>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveInterface {
    device: DeviceSelector,
    xdp: InterfaceXdp,
    #[serde(skip)]
    device_source: Source,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InterfaceXdp {
    zero_copy: bool,
    workers: WorkerPolicy,
    #[serde(skip)]
    zero_copy_source: Source,
    #[serde(skip)]
    workers_source: Source,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveModule {
    xdp: ModuleXdp,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleXdp {
    tx: ModuleTx,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleTx {
    interface: String,
    queues: QueueSelection,
    #[serde(skip)]
    queues_source: Source,
}

#[derive(Clone, Debug)]
pub(crate) struct Modules<T> {
    pub gossip: T,
    pub repair: T,
    pub tpu: T,
    pub turbine: T,
}

impl<T> Modules<T> {
    fn values(&self) -> [&T; 4] {
        [&self.gossip, &self.repair, &self.tpu, &self.turbine]
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalXdp {
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectiveConfig {
    // Validated against SCHEMA_VERSION on the raw TOML before decoding; carried
    // only so deny_unknown_fields accepts the key.
    #[serde(rename = "schema_version")]
    _schema_version: i64,
    xdp: GlobalXdp,
    interfaces: BTreeMap<String, EffectiveInterface>,
    gossip: EffectiveModule,
    repair: EffectiveModule,
    tpu: EffectiveModule,
    turbine: EffectiveModule,
}

impl EffectiveConfig {
    pub(crate) fn xdp_active(&self) -> bool {
        self.xdp.enabled
    }

    fn named_modules(&self) -> [(&'static str, &ModuleXdp); 4] {
        [
            ("gossip", &self.gossip.xdp),
            ("repair", &self.repair.xdp),
            ("tpu", &self.tpu.xdp),
            ("turbine", &self.turbine.xdp),
        ]
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CliOverrides {
    pub no_xdp: bool,
    pub interface: Option<String>,
    pub cpu_cores: Option<Vec<usize>>,
    pub zero_copy: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct CliApplication {
    pub config: EffectiveConfig,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeXdpConfig {
    pub interface_label: String,
    pub device: DeviceSelector,
    pub queues: Vec<QueueCpuBinding>,
    pub zero_copy: bool,
    pub modules: Modules<Box<[usize]>>,
}

impl<'de> Deserialize<'de> for DeviceSelector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Device {
            route: Option<String>,
            name: Option<String>,
        }

        let Device { route, name } = Device::deserialize(deserializer)?;
        match (route, name) {
            (Some(route), None) if route == "default" => Ok(Self::DefaultRoute),
            (Some(route), None) => Err(serde::de::Error::custom(format!(
                "device.route must be \"default\"; found {route:?}"
            ))),
            (None, Some(name)) => Ok(Self::Name(name)),
            (None, None) => Err(serde::de::Error::custom(
                "device must specify exactly one of device.route or device.name",
            )),
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "device specifies conflicting keys device.route and device.name",
            )),
        }
    }
}

fn validate_pool_len(len: usize, field: &str) -> Result<(), String> {
    if len == 0 || len > MAX_XDP_WORKERS {
        return Err(format!(
            "{field} must contain between 1 and {MAX_XDP_WORKERS} workers; found {len}"
        ));
    }
    Ok(())
}

fn validate_unique_cpus(cpus: &[usize], field: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    if let Some(cpu) = cpus.iter().find(|cpu| !seen.insert(**cpu)) {
        return Err(format!("{field} contains duplicate CPU {cpu}"));
    }
    Ok(())
}

impl<'de> Deserialize<'de> for WorkerPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Auto {
            count: usize,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Binding {
            queue: u32,
            cpu: usize,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Workers {
            auto: Option<Auto>,
            cpus: Option<Vec<usize>>,
            bindings: Option<Vec<Binding>>,
        }

        let workers = Workers::deserialize(deserializer)?;
        let parse = || -> Result<_, String> {
            match (workers.auto, workers.cpus, workers.bindings) {
                (None, None, None) => Err("workers must specify exactly one of workers.auto, \
                                           workers.cpus, or workers.bindings"
                    .to_string()),
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                    Err("workers specifies conflicting worker modes".to_string())
                }
                (Some(Auto { count }), None, None) => {
                    validate_pool_len(count, "workers.auto.count")?;
                    Ok(WorkerPolicy::Auto { count })
                }
                (None, Some(cpus), None) => {
                    validate_pool_len(cpus.len(), "workers.cpus")?;
                    validate_unique_cpus(&cpus, "workers.cpus")?;
                    Ok(WorkerPolicy::Cpus(cpus))
                }
                (None, None, Some(raw_bindings)) => {
                    validate_pool_len(raw_bindings.len(), "workers.bindings")?;
                    let mut queues = BTreeSet::new();
                    let mut cpus = BTreeSet::new();
                    let mut bindings = Vec::with_capacity(raw_bindings.len());
                    for Binding { queue, cpu } in raw_bindings {
                        if !queues.insert(queue) {
                            return Err(format!(
                                "workers.bindings contains duplicate queue {queue}"
                            ));
                        }
                        if !cpus.insert(cpu) {
                            return Err(format!("workers.bindings contains duplicate CPU {cpu}"));
                        }
                        bindings.push(QueueCpuBinding { queue, cpu });
                    }
                    Ok(WorkerPolicy::Bindings(bindings))
                }
            }
        };
        parse().map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for QueueSelection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        let parse = || -> Result<_, String> {
            let toml::Value::Array(values) = value else {
                return match value {
                    toml::Value::String(keyword) if keyword == "all" => Ok(Self::All),
                    toml::Value::String(keyword) => Err(format!(
                        "tx.queues accepts only \"all\" or a non-empty integer array; found \
                         {keyword:?}"
                    )),
                    other => Err(format!(
                        "tx.queues accepts only \"all\" or a non-empty integer array; found {}",
                        other.type_str()
                    )),
                };
            };
            if values.is_empty() {
                return Err("tx.queues must not be an empty queue list".to_string());
            }
            if values.len() > MAX_XDP_WORKERS {
                return Err(format!(
                    "tx.queues exceeds MAX_XDP_WORKERS ({MAX_XDP_WORKERS})"
                ));
            }
            let queues: Vec<u32> = toml::Value::Array(values)
                .try_into()
                .map_err(|error| format!("tx.queues: {error}"))?;
            let mut seen = BTreeSet::new();
            for queue in &queues {
                if !seen.insert(*queue) {
                    return Err(format!("tx.queues contains duplicate queue {queue}"));
                }
            }
            Ok(Self::Explicit(queues))
        };
        parse().map_err(serde::de::Error::custom)
    }
}

fn parse_toml(text: &str, description: &str, built_in: bool) -> Result<toml::Value, String> {
    let value: toml::Value = toml::from_str(text).map_err(|error| {
        if built_in {
            format!("built-in default config is invalid: {error}")
        } else {
            format!("invalid config file `{description}`: {error}")
        }
    })?;
    let version = match value.get("schema_version") {
        Some(value) => value.as_integer().ok_or_else(|| {
            format!(
                "config `{description}` has a non-integer schema_version; this binary supports \
                 version {SCHEMA_VERSION}"
            )
        })?,
        None if built_in => return Err("built-in config is missing schema_version".to_string()),
        None => {
            return Err(format!(
                "config `{description}` is missing required schema_version; this binary supports \
                 version {SCHEMA_VERSION}"
            ));
        }
    };
    if version != i64::from(SCHEMA_VERSION) {
        return Err(format!(
            "config `{description}` has schema_version {version}; this binary supports version \
             {SCHEMA_VERSION}"
        ));
    }
    Ok(value)
}

fn decode_config(
    value: toml::Value,
    description: &str,
    built_in: bool,
) -> Result<EffectiveConfig, String> {
    value.try_into().map_err(|error| {
        if built_in {
            format!("built-in default config is invalid: {error}")
        } else {
            format!("invalid config file `{description}`: {error}")
        }
    })
}

fn merge_value(base: &mut toml::Value, user: toml::Value, path: &mut Vec<String>) {
    let atomic = (path.len() == 3 && path[0] == "interfaces" && path[2] == "device")
        || (path.len() == 4 && path[0] == "interfaces" && path[2] == "xdp" && path[3] == "workers");
    if atomic {
        *base = user;
        return;
    }
    match (base, user) {
        (toml::Value::Table(base), toml::Value::Table(user)) => {
            if path.len() == 1 && path[0] == "interfaces" {
                let removed: Vec<_> = base
                    .keys()
                    .filter(|label| !user.contains_key(*label))
                    .cloned()
                    .collect();
                for label in removed {
                    base.remove(&label);
                }
            }
            for (key, value) in user {
                match base.get_mut(&key) {
                    Some(base) => {
                        path.push(key.clone());
                        merge_value(base, value, path);
                        path.pop();
                    }
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, user) => *base = user,
    }
}

fn mark_user_sources(config: &mut EffectiveConfig, user: &toml::Value) {
    if let Some(interfaces) = user.get("interfaces").and_then(toml::Value::as_table) {
        for (label, interface) in &mut config.interfaces {
            let Some(interface_patch) = interfaces.get(label).and_then(toml::Value::as_table)
            else {
                continue;
            };
            if interface_patch.contains_key("device") {
                interface.device_source = Source::User;
            }
            let Some(xdp) = interface_patch.get("xdp").and_then(toml::Value::as_table) else {
                continue;
            };
            if xdp.contains_key("zero_copy") {
                interface.xdp.zero_copy_source = Source::User;
            }
            if xdp.contains_key("workers") {
                interface.xdp.workers_source = Source::User;
            }
        }
    }
    for (name, module) in [
        ("gossip", &mut config.gossip.xdp),
        ("repair", &mut config.repair.xdp),
        ("tpu", &mut config.tpu.xdp),
        ("turbine", &mut config.turbine.xdp),
    ] {
        if user
            .get(name)
            .and_then(|module| module.get("xdp"))
            .and_then(|xdp| xdp.get("tx"))
            .is_some_and(|tx| tx.get("queues").is_some())
        {
            module.tx.queues_source = Source::User;
        }
    }
}

fn validate_structural(config: &EffectiveConfig) -> Result<(), String> {
    let invalid: Vec<_> = config
        .interfaces
        .iter()
        .filter_map(|(label, interface)| {
            matches!(interface.xdp.workers, WorkerPolicy::Bindings(_))
                .then_some((label, &interface.device))
        })
        .filter(|(_, device)| !matches!(device, DeviceSelector::Name(_)))
        .map(|(label, _)| label.as_str())
        .collect();
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "workers.bindings requires device.name in the merged file policy for interface(s) {}; \
             use workers.auto/workers.cpus with device.route, or name the device",
            invalid.join(", ")
        ))
    }
}

pub(crate) fn load(user_path: Option<&Path>) -> Result<EffectiveConfig, String> {
    let mut built_in = parse_toml(DEFAULT_CONFIG, "<built-in>", true)?;
    let base = decode_config(built_in.clone(), "<built-in>", true)?;
    validate_structural(&base)?;
    match user_path {
        None => Ok(base),
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|error| {
                format!("failed to read config file `{}`: {error}", path.display())
            })?;
            let description = path.display().to_string();
            let user = parse_toml(&text, &description, false)?;
            merge_value(&mut built_in, user.clone(), &mut Vec::new());
            let mut config = decode_config(built_in, &description, false)?;
            mark_user_sources(&mut config, &user);
            validate_structural(&config)?;
            Ok(config)
        }
    }
}

pub(crate) fn apply_cli(
    mut config: EffectiveConfig,
    overrides: CliOverrides,
) -> Result<CliApplication, String> {
    if overrides.no_xdp {
        config.xdp.enabled = false;
    }
    let mut warnings = Vec::new();
    if !config.xdp_active() {
        if let Some(interface) = overrides.interface {
            warnings.push(format!(
                "runtime XDP is inactive; ignoring --xdp-interface={interface}"
            ));
        }
        if let Some(cpus) = overrides.cpu_cores {
            warnings.push(format!(
                "runtime XDP is inactive; ignoring --xdp-cpu-cores={}",
                cpus.iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if let Some(zero_copy) = overrides.zero_copy {
            warnings.push(format!(
                "runtime XDP is inactive; ignoring {}",
                if zero_copy {
                    "--xdp-zero-copy"
                } else {
                    "--no-xdp-zero-copy"
                }
            ));
        }
        warnings.sort();
        return Ok(CliApplication { config, warnings });
    }
    if config.interfaces.len() != 1 {
        return Err(format!(
            "XDP currently supports exactly one effective interface; found {}",
            config.interfaces.len()
        ));
    }
    if let Some(cpus) = overrides.cpu_cores.as_ref() {
        validate_pool_len(cpus.len(), "--xdp-cpu-cores")?;
        validate_unique_cpus(cpus, "--xdp-cpu-cores")?;
        let interface = config.interfaces.values().next().unwrap();
        if interface.xdp.workers_source == Source::User {
            let affected: Vec<_> = config
                .named_modules()
                .into_iter()
                .filter(|(_, module)| {
                    module.tx.queues_source == Source::User
                        && matches!(module.tx.queues, QueueSelection::Explicit(_))
                })
                .map(|(name, _)| name)
                .collect();
            if !affected.is_empty() {
                return Err(format!(
                    "--xdp-cpu-cores replaces user-authored workers and would reinterpret \
                     user-authored numeric queue ids in module(s) {}; use tx.queues = \"all\" or \
                     update the file and CLI choices together",
                    affected.join(", ")
                ));
            }
        }
    }
    let (label, interface) = config.interfaces.iter_mut().next().unwrap();
    if let Some(cpus) = overrides.cpu_cores {
        if interface.xdp.workers_source == Source::User {
            warnings.push(format!(
                "--xdp-cpu-cores replaces user-authored workers for interface {label:?}"
            ));
        }
        interface.xdp.workers = WorkerPolicy::Cpus(cpus);
        interface.xdp.workers_source = Source::Cli;
    }
    if let Some(name) = overrides.interface {
        let same = matches!(&interface.device, DeviceSelector::Name(old) if old == &name);
        if matches!(&interface.xdp.workers, WorkerPolicy::Bindings(_)) && !same {
            return Err(format!(
                "--xdp-interface changes the device while workers.bindings is active for \
                 interface {label:?}; also supply --xdp-cpu-cores or update workers.bindings"
            ));
        }
        if !same {
            if interface.device_source == Source::User {
                warnings.push(format!(
                    "--xdp-interface replaces the user-authored device selector for interface \
                     {label:?}"
                ));
            }
            interface.device = DeviceSelector::Name(name);
            interface.device_source = Source::Cli;
        }
    }
    if let Some(zero_copy) = overrides.zero_copy {
        if interface.xdp.zero_copy_source == Source::User {
            warnings.push(format!(
                "{} replaces user-authored {}.xdp.zero_copy",
                if zero_copy {
                    "--xdp-zero-copy"
                } else {
                    "--no-xdp-zero-copy"
                },
                interface_path(label)
            ));
        }
        interface.xdp.zero_copy = zero_copy;
        interface.xdp.zero_copy_source = Source::Cli;
    }
    Ok(CliApplication { config, warnings })
}

/// Queue ids a module transmits over, in its own sender order.
fn module_queue_ids<'a>(module: &'a ModuleXdp, pool: &'a [u32]) -> &'a [u32] {
    match &module.tx.queues {
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

/// Validate host-independent cross-references. Problems are fatal when XDP is
/// enabled; dormant-policy problems are reported as warnings.
pub(crate) fn validate_policy(config: &EffectiveConfig) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    let active = config.xdp_active();
    if active && config.interfaces.len() != 1 {
        return Err(format!(
            "XDP version 1 supports exactly one effective interface; found {}",
            config.interfaces.len()
        ));
    }
    if config.interfaces.len() != 1 {
        return Ok(warnings);
    }
    let (label, interface) = config.interfaces.iter().next().unwrap();
    for (name, module) in config.named_modules() {
        if module.tx.interface == *label {
            continue;
        }
        let message = format!(
            "{name}.xdp.tx.interface names {:?}, which is not a declared interface; declared: {:?}",
            module.tx.interface, label
        );
        if active {
            return Err(message);
        }
        warnings.push(message);
    }
    let pool = worker_queue_ids(&interface.xdp.workers);
    let pool_set: BTreeSet<_> = pool.iter().copied().collect();
    for (name, module) in config.named_modules() {
        if let QueueSelection::Explicit(queues) = &module.tx.queues {
            let missing: Vec<_> = queues
                .iter()
                .filter(|queue| !pool_set.contains(queue))
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
    let selections: Vec<_> = config
        .named_modules()
        .into_iter()
        .map(|(_, module)| module_queue_ids(module, &pool))
        .collect();
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
                .collect();
            if eligible.len() < *count {
                return Err(format!(
                    "workers.auto.count = {count} requires {count} eligible CPUs, but only {} \
                     remain after excluding PoH",
                    eligible.len()
                ));
            }
            eligible.into_iter().take(*count).collect()
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

pub(crate) fn resolve_runtime(
    config: &EffectiveConfig,
    allowed_cpus: &BTreeSet<usize>,
    poh_core: Option<usize>,
) -> Result<(RuntimeXdpConfig, Vec<String>), String> {
    let warnings = validate_policy(config)?;
    if !config.xdp_active() {
        return Err("cannot resolve an inactive XDP policy".to_string());
    }
    let (label, interface) = config.interfaces.iter().next().unwrap();
    let declared = resolve_declared_workers(&interface.xdp.workers, allowed_cpus, poh_core)?;
    let pool: Vec<u32> = declared.iter().map(|binding| binding.queue).collect();
    let selected = Modules {
        gossip: module_queue_ids(&config.gossip.xdp, &pool),
        repair: module_queue_ids(&config.repair.xdp, &pool),
        tpu: module_queue_ids(&config.tpu.xdp, &pool),
        turbine: module_queue_ids(&config.turbine.xdp, &pool),
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
    let module_positions = |queues: &[u32]| -> Box<[usize]> {
        queues
            .iter()
            .map(|queue| {
                *positions
                    .get(queue)
                    .expect("validated selected queue must have an active worker")
            })
            .collect()
    };
    let modules = Modules {
        gossip: module_positions(selected.gossip),
        repair: module_positions(selected.repair),
        tpu: module_positions(selected.tpu),
        turbine: module_positions(selected.turbine),
    };
    Ok((
        RuntimeXdpConfig {
            interface_label: label.clone(),
            device: interface.device.clone(),
            queues: active_workers,
            zero_copy: interface.xdp.zero_copy,
            modules,
        },
        warnings,
    ))
}

fn interface_path(label: &str) -> String {
    let key = if !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        label.to_string()
    } else {
        toml::Value::String(label.to_string()).to_string()
    };
    format!("interfaces.{key}")
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Write as _};

    const ALL_MODULES_QUEUE_ZERO: &str = r#"
schema_version = 1
tpu.xdp.tx.queues = [0]
turbine.xdp.tx.queues = [0]
repair.xdp.tx.queues = [0]
gossip.xdp.tx.queues = [0]
"#;

    fn load_config(contents: &str) -> Result<EffectiveConfig, String> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        load(Some(file.path()))
    }

    fn load_valid_config(contents: &str) -> EffectiveConfig {
        load_config(contents)
            .unwrap_or_else(|error| panic!("invalid test config:\n{contents}\n{error}"))
    }

    fn load_worker_config(workers: &str) -> Result<EffectiveConfig, String> {
        load_config(&format!(
            r#"
schema_version = 1
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
workers = {workers}
"#
        ))
    }

    // Each fragment group must match one warning; warning order does not matter.
    fn assert_warnings_contain(warnings: &[String], expected: &[&[&str]], case: &str) {
        assert_eq!(warnings.len(), expected.len(), "{case}: {warnings:?}");
        let mut unmatched: Vec<_> = warnings.iter().collect();
        for fragments in expected {
            let index = unmatched
                .iter()
                .position(|warning| fragments.iter().all(|fragment| warning.contains(*fragment)))
                .unwrap_or_else(|| {
                    panic!("{case}: expected warning containing {fragments:?}; got {warnings:?}")
                });
            unmatched.remove(index);
        }
    }

    #[test]
    fn embedded_default_resolves_from_policy_without_fallbacks() {
        let config = load(None).unwrap();
        assert!(config.xdp.enabled);
        assert_eq!(config.interfaces.len(), 1);
        let interface = &config.interfaces["primary"];
        assert_eq!(interface.device, DeviceSelector::DefaultRoute);
        assert_eq!(interface.xdp.workers, WorkerPolicy::Auto { count: 1 });
        let allowed = BTreeSet::from([1, 3, 5]);
        let (runtime, warnings) = resolve_runtime(&config, &allowed, Some(5)).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(runtime.queues, vec![QueueCpuBinding { queue: 0, cpu: 3 }]);
        for module in runtime.modules.values() {
            assert_eq!(module.as_ref(), &[0][..]);
        }
    }

    #[test]
    fn scalar_patch_inherits_atomic_choices() {
        let config = load_valid_config(
            r#"
schema_version = 1
[interfaces.primary.xdp]
zero_copy = true
"#,
        );
        let interface = &config.interfaces["primary"];
        assert!(interface.xdp.zero_copy);
        assert_eq!(interface.device, DeviceSelector::DefaultRoute);
        assert_eq!(interface.xdp.workers, WorkerPolicy::Auto { count: 1 });
    }

    #[test]
    fn new_interface_error_identifies_missing_field() {
        let error = load_config(
            r#"
schema_version = 1
[interfaces.fast.xdp]
zero_copy = false
"#,
        )
        .unwrap_err();
        assert!(error.contains("missing field `workers`"), "{error}");
        assert!(error.contains("interfaces.fast.xdp"), "{error}");
    }

    #[test]
    fn workers_replace_atomically() {
        let config = load_valid_config(
            r#"
schema_version = 1
[interfaces.primary.xdp]
workers.cpus = [8, 9]
"#,
        );
        assert_eq!(
            config.interfaces["primary"].xdp.workers,
            WorkerPolicy::Cpus(vec![8, 9])
        );
    }

    #[test]
    fn invalid_selectors_are_rejected_after_merge() {
        for label in ["primary", "fast"] {
            for (contents, expected) in [
                (
                    format!(
                        r#"
schema_version = 1
[interfaces.{label}]
device = {{ route = "default", name = "eth0" }}
"#
                    ),
                    "conflicting keys device.route and device.name",
                ),
                (
                    format!(
                        r#"
schema_version = 1
[interfaces.{label}]
device.route = "other"
"#
                    ),
                    "device.route must be \"default\"",
                ),
                (
                    format!(
                        r#"
schema_version = 1
[interfaces.{label}]
device = {{}}
"#
                    ),
                    "device must specify exactly one",
                ),
                (
                    format!(
                        r#"
schema_version = 1
[interfaces.{label}.xdp]
workers = {{ auto = {{ count = 1 }}, cpus = [8] }}
"#
                    ),
                    "conflicting worker modes",
                ),
            ] {
                let error = load_config(&contents).unwrap_err();
                assert!(error.contains(expected), "{contents}: {error}");
            }
        }
    }

    #[test]
    fn invalid_worker_policies_are_rejected() {
        for (workers, expected) in [
            ("{}", "workers must specify exactly one"),
            ("{ unused = \"warn\" }", "unknown field `unused`"),
            (
                "{ cpus = [8, 9, 8] }",
                "workers.cpus contains duplicate CPU 8",
            ),
            (
                "{ bindings = [{ queue = 3, cpu = 8 }, { queue = 3, cpu = 9 }] }",
                "workers.bindings contains duplicate queue 3",
            ),
            (
                "{ bindings = [{ queue = 3, cpu = 8 }, { queue = 7, cpu = 8 }] }",
                "workers.bindings contains duplicate CPU 8",
            ),
            (
                "{ auto = { count = 1 }, bindings = [{ queue = 0, cpu = 8 }] }",
                "conflicting worker modes",
            ),
            (
                "{ cpus = [8], bindings = [{ queue = 0, cpu = 8 }] }",
                "conflicting worker modes",
            ),
        ] {
            let error = load_worker_config(workers).unwrap_err();
            assert!(error.contains(expected), "{workers}: {error}");
        }
    }

    #[test]
    fn bindings_require_named_device_before_cli() {
        let error = load_config(
            r#"
schema_version = 1
[interfaces.primary.xdp]
workers.bindings = [{ queue = 0, cpu = 8 }]
"#,
        )
        .unwrap_err();
        assert!(
            error.contains("workers.bindings requires device.name"),
            "{error}"
        );
    }

    #[test]
    fn invalid_schema_versions_are_rejected() {
        for (contents, expected) in [
            ("[xdp]\nenabled = false", "missing required schema_version"),
            ("schema_version = 2", "supports version 1"),
            ("schema_version = \"one\"", "non-integer schema_version"),
        ] {
            let error = load_config(contents).unwrap_err();
            assert!(error.contains(expected), "{contents}: {error}");
        }
    }

    #[test]
    fn invalid_queue_reference_warns_when_dormant_and_fails_when_active() {
        let mut config = load_valid_config(
            r#"
schema_version = 1
[xdp]
enabled = false
[tpu.xdp]
tx.queues = [1]
"#,
        );
        let expected = [
            "tpu.xdp.tx.queues",
            " 1 ",
            "not declared",
            "interfaces.primary.xdp.workers",
        ];
        assert_warnings_contain(
            &validate_policy(&config).unwrap(),
            &[&expected],
            "dormant policy",
        );
        config.xdp.enabled = true;
        let error = resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap_err();
        assert!(
            expected.iter().all(|fragment| error.contains(fragment)),
            "{error}"
        );
    }

    #[test]
    fn every_worker_mode_enforces_the_same_cardinality_limit() {
        for (count, should_succeed) in [
            (0, false),
            (1, true),
            (MAX_XDP_WORKERS, true),
            (MAX_XDP_WORKERS + 1, false),
        ] {
            // Use structured arrays for the cases with thousands of workers.
            let cpus: Vec<_> = (0..count).collect();
            let bindings = (0..count)
                .map(|cpu| {
                    toml::toml! {
                        queue = (cpu)
                        cpu = (cpu)
                    }
                })
                .collect::<Vec<_>>();
            for (field, policy) in [
                ("auto.count", toml::toml! { auto.count = (count) }),
                ("cpus", toml::toml! { cpus = (cpus) }),
                ("bindings", toml::toml! { bindings = (bindings) }),
            ] {
                let result = policy.try_into::<WorkerPolicy>();
                if should_succeed {
                    result.unwrap_or_else(|error| panic!("{field}/{count}: {error}"));
                } else {
                    let error = result.unwrap_err().to_string();
                    let expected = format!(
                        "workers.{field} must contain between 1 and {MAX_XDP_WORKERS} workers; \
                         found {count}"
                    );
                    assert!(error.contains(&expected), "{field}/{count}: {error}");
                }
            }
        }
    }

    #[test]
    fn renaming_the_interface_requires_updating_module_references() {
        const RENAMED: &str = r#"
schema_version = 1
[interfaces.fast]
device.name = "eth0"
[interfaces.fast.xdp]
zero_copy = false
workers.cpus = [8]
"#;
        let error = validate_policy(&load_valid_config(RENAMED)).unwrap_err();
        assert!(error.contains("not a declared interface"), "{error}");

        let updated_references = load_valid_config(
            r#"
schema_version = 1
[interfaces.fast]
device.name = "eth0"
[interfaces.fast.xdp]
zero_copy = false
workers.cpus = [8]
[tpu.xdp]
tx.interface = "fast"
[turbine.xdp]
tx.interface = "fast"
[repair.xdp]
tx.interface = "fast"
[gossip.xdp]
tx.interface = "fast"
"#,
        );
        let (runtime, _) =
            resolve_runtime(&updated_references, &BTreeSet::from([8, 9]), None).unwrap();
        assert_eq!(runtime.interface_label, "fast");
        for module in runtime.modules.values() {
            assert_eq!(module.as_ref(), &[0][..]);
        }
    }

    #[test]
    fn module_referencing_another_interface_is_rejected() {
        let config = load_valid_config(
            r#"
schema_version = 1
[tpu.xdp]
tx.interface = "other"
"#,
        );
        let error = validate_policy(&config).unwrap_err();
        assert_eq!(
            error,
            "tpu.xdp.tx.interface names \"other\", which is not a declared interface; declared: \
             \"primary\""
        );
    }

    #[test]
    fn cli_worker_replacement_rejects_user_queue_ids_even_if_they_survive() {
        let config = load_valid_config(
            r#"
schema_version = 1
[interfaces.primary.xdp]
workers.cpus = [8, 9]
[tpu.xdp]
tx.queues = [0]
"#,
        );
        let error = apply_cli(
            config,
            CliOverrides {
                cpu_cores: Some(vec![10, 11]),
                ..CliOverrides::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("reinterpret"), "{error}");
    }

    #[test]
    fn module_level_switches_are_rejected() {
        for module in ["tpu", "turbine", "repair", "gossip"] {
            let error = load_config(&format!(
                r#"
schema_version = 1
[{module}.xdp]
enabled = true
"#
            ))
            .unwrap_err();
            assert!(
                error.contains("unknown field `enabled`"),
                "{module}: {error}"
            );
        }
    }

    #[test]
    fn cli_cpu_workers_preserve_module_queue_scoping() {
        let config = load_valid_config(
            r#"
schema_version = 1
[tpu.xdp]
tx.queues = [0]
"#,
        );
        let application = apply_cli(
            config,
            CliOverrides {
                cpu_cores: Some(vec![8, 9]),
                ..CliOverrides::default()
            },
        )
        .unwrap();
        let (runtime, _) =
            resolve_runtime(&application.config, &BTreeSet::from([8, 9, 10]), None).unwrap();
        assert_eq!(runtime.modules.tpu.as_ref(), &[0][..]);
        assert_eq!(runtime.modules.turbine.as_ref(), &[0, 1][..]);
    }

    #[test]
    fn sparse_bindings_preserve_worker_and_module_order() {
        // Each module specifies its queue selection and expected sender positions.
        for (case, modules, expected_workers) in [
            (
                "all workers",
                Modules {
                    gossip: ("[11, 1]", vec![2, 3]),
                    repair: ("[3]", vec![1]),
                    tpu: ("[1, 7]", vec![3, 0]),
                    turbine: ("'all'", vec![0, 1, 2, 3]),
                },
                vec![(7, 8), (3, 9), (11, 10), (1, 11)],
            ),
            (
                "unused middle workers",
                Modules {
                    gossip: ("[7, 1]", vec![0, 1]),
                    repair: ("[1]", vec![1]),
                    tpu: ("[1, 7]", vec![1, 0]),
                    turbine: ("[7]", vec![0]),
                },
                vec![(7, 8), (1, 11)],
            ),
        ] {
            let Modules {
                gossip: (gossip, expected_gossip),
                repair: (repair, expected_repair),
                tpu: (tpu, expected_tpu),
                turbine: (turbine, expected_turbine),
            } = modules;
            let contents = format!(
                r#"
schema_version = 1
gossip.xdp.tx.queues = {gossip}
repair.xdp.tx.queues = {repair}
tpu.xdp.tx.queues = {tpu}
turbine.xdp.tx.queues = {turbine}
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
workers.bindings = [
    {{ queue = 7, cpu = 8 }},
    {{ queue = 3, cpu = 9 }},
    {{ queue = 11, cpu = 10 }},
    {{ queue = 1, cpu = 11 }},
]
"#
            );
            let (runtime, _) = resolve_runtime(
                &load_valid_config(&contents),
                &BTreeSet::from([8, 9, 10, 11, 12]),
                None,
            )
            .unwrap();
            let expected_workers: Vec<_> = expected_workers
                .into_iter()
                .map(|(queue, cpu)| QueueCpuBinding { queue, cpu })
                .collect();
            assert_eq!(runtime.queues, expected_workers, "{case}");
            assert_eq!(
                runtime.modules.gossip.as_ref(),
                expected_gossip,
                "{case}/gossip"
            );
            assert_eq!(
                runtime.modules.repair.as_ref(),
                expected_repair,
                "{case}/repair"
            );
            assert_eq!(runtime.modules.tpu.as_ref(), expected_tpu, "{case}/tpu");
            assert_eq!(
                runtime.modules.turbine.as_ref(),
                expected_turbine,
                "{case}/turbine"
            );
        }
    }

    #[test]
    fn cli_device_overrides_respect_explicit_bindings() {
        use Source::{Cli, User};

        const DEVICE: &str = r#"
schema_version = 1
[interfaces.primary]
device.name = "eth0"
"#;
        const BINDINGS: &str = r#"
schema_version = 1
[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
workers.bindings = [{ queue = 3, cpu = 8 }]
[tpu.xdp]
tx.queues = "all"
"#;
        const DEVICE_WARNING: &[&str] =
            &["--xdp-interface", "replaces", "user-authored", "primary"];
        const WORKER_WARNING: &[&str] =
            &["--xdp-cpu-cores", "replaces", "user-authored", "primary"];
        const DEVICE_CHANGE_ERROR: &str =
            "--xdp-interface changes the device while workers.bindings is active";
        for (contents, name, cpu_cores, expected_source, warnings) in [
            ("schema_version = 1", "eth1", None, Ok(Cli), vec![]),
            (DEVICE, "eth1", None, Ok(Cli), vec![DEVICE_WARNING]),
            (BINDINGS, "eth0", None, Ok(User), vec![]),
            (BINDINGS, "eth1", None, Err(DEVICE_CHANGE_ERROR), vec![]),
            (
                BINDINGS,
                "eth1",
                Some(vec![9, 10]),
                Ok(Cli),
                vec![WORKER_WARNING, DEVICE_WARNING],
            ),
            (
                BINDINGS,
                "eth0",
                Some(vec![9, 10]),
                Ok(User),
                vec![WORKER_WARNING],
            ),
        ] {
            let case = format!("device={name}, CPUs={cpu_cores:?}, config:\n{contents}");
            let config = load_valid_config(contents);
            let original_workers = config.interfaces["primary"].xdp.workers.clone();
            let application = apply_cli(
                config,
                CliOverrides {
                    interface: Some(name.to_string()),
                    cpu_cores: cpu_cores.clone(),
                    ..CliOverrides::default()
                },
            );
            let source = match expected_source {
                Ok(source) => source,
                Err(expected_error) => {
                    let error = application.unwrap_err();
                    assert!(error.contains(expected_error), "{case}: {error}");
                    continue;
                }
            };
            let application = application.unwrap_or_else(|error| panic!("{case}: {error}"));
            let interface = &application.config.interfaces["primary"];
            assert_eq!(
                interface.device,
                DeviceSelector::Name(name.to_string()),
                "{case}"
            );
            assert_eq!(interface.device_source, source, "{case}");
            assert_warnings_contain(&application.warnings, &warnings, &case);
            let expected_workers = if let Some(cpus) = cpu_cores {
                assert_eq!(interface.xdp.workers_source, Cli, "{case}");
                WorkerPolicy::Cpus(cpus)
            } else {
                original_workers
            };
            assert_eq!(interface.xdp.workers, expected_workers, "{case}");
        }
    }

    #[test]
    fn cli_zero_copy_overrides_file_and_default_values() {
        use Source::{BuiltIn, Cli, User};

        for (file_value, cli_value, expected_value, expected_source) in [
            (None, None, false, BuiltIn),
            (None, Some(false), false, Cli),
            (None, Some(true), true, Cli),
            (Some(false), None, false, User),
            (Some(false), Some(false), false, Cli),
            (Some(false), Some(true), true, Cli),
            (Some(true), None, true, User),
            (Some(true), Some(false), false, Cli),
            (Some(true), Some(true), true, Cli),
        ] {
            let config = match file_value {
                None => load(None).unwrap(),
                Some(value) => load_valid_config(&format!(
                    r#"
schema_version = 1
[interfaces.primary.xdp]
zero_copy = {value}
"#
                )),
            };
            let application = apply_cli(
                config,
                CliOverrides {
                    zero_copy: cli_value,
                    ..CliOverrides::default()
                },
            )
            .unwrap();
            let case = format!("file={file_value:?}, CLI={cli_value:?}");
            let interface = &application.config.interfaces["primary"];
            assert_eq!(interface.xdp.zero_copy, expected_value, "{case}");
            assert_eq!(interface.xdp.zero_copy_source, expected_source, "{case}");
            match (file_value, cli_value) {
                (Some(_), Some(value)) => {
                    let flag = if value {
                        "--xdp-zero-copy"
                    } else {
                        "--no-xdp-zero-copy"
                    };
                    assert_warnings_contain(
                        &application.warnings,
                        &[&[
                            flag,
                            "replaces",
                            "user-authored",
                            "interfaces.primary.xdp.zero_copy",
                        ]],
                        &case,
                    );
                }
                _ => assert!(
                    application.warnings.is_empty(),
                    "{case}: {:?}",
                    application.warnings
                ),
            }
            let (runtime, _) =
                resolve_runtime(&application.config, &BTreeSet::from([8, 9]), None).unwrap();
            assert_eq!(runtime.zero_copy, expected_value, "{case}");
        }
    }

    #[test]
    fn cli_cpu_workers_reject_duplicate_cpus() {
        let error = apply_cli(
            load(None).unwrap(),
            CliOverrides {
                cpu_cores: Some(vec![8, 9, 8]),
                ..CliOverrides::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("duplicate CPU 8"), "{error}");
    }

    #[test]
    fn explicit_workers_reject_invalid_cpus() {
        for (case, config) in [
            ("cpus", load_worker_config("{ cpus = [9] }").unwrap()),
            (
                "bindings",
                load_worker_config("{ bindings = [{ queue = 0, cpu = 9 }] }").unwrap(),
            ),
            (
                "CLI override",
                apply_cli(
                    load(None).unwrap(),
                    CliOverrides {
                        cpu_cores: Some(vec![9]),
                        ..CliOverrides::default()
                    },
                )
                .unwrap()
                .config,
            ),
        ] {
            for (allowed, poh, expected) in [
                (
                    BTreeSet::from([8, 9, 10]),
                    Some(9),
                    "XDP worker CPU 9 overlaps the PoH core",
                ),
                (
                    BTreeSet::from([8, 10]),
                    None,
                    "XDP worker CPU 9 is not in the process CPU-affinity set",
                ),
            ] {
                let error = resolve_runtime(&config, &allowed, poh).unwrap_err();
                assert_eq!(error, expected, "{case}");
            }
        }
    }

    #[test]
    fn auto_workers_require_enough_cpus_after_excluding_poh() {
        let config = load_worker_config("{ auto = { count = 2 } }").unwrap();
        let error = resolve_runtime(&config, &BTreeSet::from([8, 9]), Some(9)).unwrap_err();
        assert_eq!(
            error,
            "workers.auto.count = 2 requires 2 eligible CPUs, but only 1 remain after excluding \
             PoH"
        );
    }

    #[test]
    fn every_worker_mode_must_leave_a_cpu_unreserved() {
        for workers in [
            "{ auto = { count = 2 } }",
            "{ cpus = [8, 9] }",
            "{ bindings = [{ queue = 0, cpu = 8 }, { queue = 1, cpu = 9 }] }",
        ] {
            let config = load_worker_config(workers).unwrap();
            let error = resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap_err();
            assert!(error.contains("leave at least one"), "{workers}: {error}");
        }
    }

    #[test]
    fn inactive_cli_overrides_are_ignored_without_topology_validation() {
        let config = load_valid_config(
            r#"
schema_version = 1
[xdp]
enabled = false

[interfaces.one]
device.route = "default"
[interfaces.one.xdp]
zero_copy = false
workers.auto.count = 1

[interfaces.two]
device.name = "eth0"
[interfaces.two.xdp]
zero_copy = false
workers.auto.count = 1
"#,
        );
        for (overrides, flag) in [
            (
                CliOverrides {
                    interface: Some("eth1".to_string()),
                    ..CliOverrides::default()
                },
                "--xdp-interface=eth1",
            ),
            (
                CliOverrides {
                    cpu_cores: Some(vec![8, 9]),
                    ..CliOverrides::default()
                },
                "--xdp-cpu-cores=8,9",
            ),
            (
                CliOverrides {
                    zero_copy: Some(true),
                    ..CliOverrides::default()
                },
                "--xdp-zero-copy",
            ),
            (
                CliOverrides {
                    zero_copy: Some(false),
                    ..CliOverrides::default()
                },
                "--no-xdp-zero-copy",
            ),
        ] {
            let application = apply_cli(config.clone(), overrides).unwrap();
            assert_warnings_contain(
                &application.warnings,
                &[&[flag, "inactive", "ignoring"]],
                flag,
            );
            assert_eq!(application.config.interfaces.len(), 2);
            for (label, original) in &config.interfaces {
                let interface = &application.config.interfaces[label];
                assert_eq!(interface.device, original.device, "{flag}/{label}");
                assert_eq!(
                    interface.xdp.workers, original.xdp.workers,
                    "{flag}/{label}"
                );
                assert_eq!(
                    interface.xdp.zero_copy, original.xdp.zero_copy,
                    "{flag}/{label}"
                );
            }
            assert!(!application.config.xdp_active());
            assert!(validate_policy(&application.config).is_ok());
        }
    }

    #[test]
    fn unreferenced_workers_warn_and_release_cpus_for_every_source() {
        let mut built_in_workers = load_valid_config(ALL_MODULES_QUEUE_ZERO);
        // Simulate a built-in pool with an unused worker, retaining its provenance.
        built_in_workers
            .interfaces
            .get_mut("primary")
            .unwrap()
            .xdp
            .workers = WorkerPolicy::Cpus(vec![8, 9]);
        let user_workers = load_valid_config(
            r#"
schema_version = 1
interfaces.primary.xdp.workers.cpus = [8, 9]
tpu.xdp.tx.queues = [0]
turbine.xdp.tx.queues = [0]
repair.xdp.tx.queues = [0]
gossip.xdp.tx.queues = [0]
"#,
        );
        let cli_workers = apply_cli(
            load_valid_config(ALL_MODULES_QUEUE_ZERO),
            CliOverrides {
                cpu_cores: Some(vec![8, 9]),
                ..CliOverrides::default()
            },
        )
        .unwrap()
        .config;
        for (source, config) in [
            ("built-in", built_in_workers),
            ("user-authored", user_workers),
            ("CLI-authored", cli_workers),
        ] {
            let (runtime, warnings) =
                resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap();
            assert_eq!(
                runtime.queues,
                [QueueCpuBinding { queue: 0, cpu: 8 }],
                "{source}"
            );
            assert_warnings_contain(
                &warnings,
                &[&[source, "queue 1 ", "primary", "unreferenced"]],
                source,
            );
        }
    }

    #[test]
    fn invalid_queue_selections_are_rejected() {
        let too_many: Vec<_> = (0..=MAX_XDP_WORKERS).collect();
        for (queues, expected) in [
            ("true", "accepts only \"all\" or a non-empty integer array"),
            ("\"other\"", "found \"other\""),
            ("[]", "tx.queues must not be an empty queue list"),
            ("[3, 3]", "tx.queues contains duplicate queue 3"),
            ("[-1]", "tx.queues: invalid value"),
            ("[4294967296]", "tx.queues: invalid value"),
            ("[\"zero\"]", "tx.queues: invalid type"),
            (
                &format!("{too_many:?}"),
                "tx.queues exceeds MAX_XDP_WORKERS",
            ),
        ] {
            let error = load_config(&format!(
                r#"
schema_version = 1
[tpu.xdp]
tx.queues = {queues}
"#
            ))
            .unwrap_err();
            assert!(error.contains(expected), "{queues}: {error}");
        }
    }
}
