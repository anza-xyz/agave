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
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

/// The embedded default policy, as shipped, for `--print-default-config`.
pub(crate) fn default_config_toml() -> &'static str {
    DEFAULT_CONFIG
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Source {
    #[default]
    BuiltIn,
    User,
    Cli,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceSelector {
    DefaultRoute,
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkerPolicy {
    Auto { count: usize },
    Cpus(Vec<usize>),
    Bindings(Vec<QueueCpuBinding>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum QueueSelection {
    All,
    Explicit(Vec<u32>),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EffectiveInterface {
    device: DeviceSelector,
    xdp: InterfaceXdp,
    #[serde(skip)]
    device_source: Source,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct InterfaceXdp {
    zero_copy: bool,
    workers: WorkerPolicy,
    #[serde(skip)]
    zero_copy_source: Source,
    #[serde(skip)]
    workers_source: Source,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EffectiveModule {
    xdp: ModuleXdp,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ModuleXdp {
    enabled: bool,
    tx: ModuleTx,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ModuleTx {
    interface: String,
    queues: QueueSelection,
    #[serde(skip)]
    interface_source: Source,
    #[serde(skip)]
    queues_source: Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct GlobalXdp {
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectiveConfig {
    // Validated against SCHEMA_VERSION on the raw TOML before decoding; carried
    // only so deny_unknown_fields accepts the key.
    schema_version: i64,
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
            && self
                .named_modules()
                .into_iter()
                .any(|(_, module)| module.enabled)
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeXdpConfig {
    pub interface_label: String,
    pub device: DeviceSelector,
    pub queues: Vec<QueueCpuBinding>,
    pub zero_copy: bool,
    pub modules: Modules<Option<Box<[usize]>>>,
}

impl<'de> Deserialize<'de> for DeviceSelector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Device {
            route: Option<String>,
            name: Option<String>,
        }

        match Device::deserialize(deserializer)? {
            Device {
                route: Some(route),
                name: None,
            } if route == "default" => Ok(Self::DefaultRoute),
            Device {
                route: Some(route),
                name: None,
            } => Err(serde::de::Error::custom(format!(
                "device.route must be \"default\"; found {route:?}"
            ))),
            Device {
                route: None,
                name: Some(name),
            } => {
                validate_device_name(&name, "device.name").map_err(serde::de::Error::custom)?;
                Ok(Self::Name(name))
            }
            Device {
                route: None,
                name: None,
            } => Err(serde::de::Error::custom(
                "device must specify exactly one of device.route or device.name",
            )),
            Device {
                route: Some(_),
                name: Some(_),
            } => Err(serde::de::Error::custom(
                "device specifies conflicting keys device.route and device.name",
            )),
        }
    }
}

fn checked_queue(value: i64, field: &str) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| {
        format!(
            "{field} value {value} is outside the supported range 0..={}",
            u32::MAX
        )
    })
}

fn checked_usize(value: i64, field: &str) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| {
        format!("{field} value {value} is outside the supported non-negative usize range")
    })
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

fn validate_device_name(name: &str, field: &str) -> Result<(), String> {
    // Linux IFNAMSIZ includes the terminating NUL.
    let invalid = name.is_empty()
        || name == "."
        || name == ".."
        || name.len() >= 16
        || name
            .bytes()
            .any(|byte| byte == 0 || byte == b'/' || byte == b':' || byte.is_ascii_whitespace());
    if invalid {
        return Err(format!(
            "{field} value {name:?} is not a valid platform interface name"
        ));
    }
    Ok(())
}

impl<'de> Deserialize<'de> for WorkerPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Auto {
            count: i64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Binding {
            queue: i64,
            cpu: i64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Workers {
            auto: Option<Auto>,
            cpus: Option<Vec<i64>>,
            bindings: Option<Vec<Binding>>,
        }

        let workers = Workers::deserialize(deserializer)?;
        let parse = || -> Result<_, String> {
            let policy = match (workers.auto, workers.cpus, workers.bindings) {
                (None, None, None) => Err("workers must specify exactly one of workers.auto, \
                                           workers.cpus, or workers.bindings"
                    .to_string()),
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                    Err("workers specifies conflicting worker modes".to_string())
                }
                (Some(auto), None, None) => {
                    let count = checked_usize(auto.count, "workers.auto.count")?;
                    validate_pool_len(count, "workers.auto.count")?;
                    Ok(WorkerPolicy::Auto { count })
                }
                (None, Some(raw_cpus), None) => {
                    validate_pool_len(raw_cpus.len(), "workers.cpus")?;
                    let mut cpus = Vec::with_capacity(raw_cpus.len());
                    for (index, value) in raw_cpus.into_iter().enumerate() {
                        let cpu = checked_usize(value, &format!("workers.cpus[{index}]"))?;
                        cpus.push(cpu);
                    }
                    validate_unique_cpus(&cpus, "workers.cpus")?;
                    Ok(WorkerPolicy::Cpus(cpus))
                }
                (None, None, Some(raw_bindings)) => {
                    validate_pool_len(raw_bindings.len(), "workers.bindings")?;
                    let mut queues = BTreeSet::new();
                    let mut cpus = BTreeSet::new();
                    let mut bindings = Vec::with_capacity(raw_bindings.len());
                    for (index, binding) in raw_bindings.into_iter().enumerate() {
                        let queue = checked_queue(
                            binding.queue,
                            &format!("workers.bindings[{index}].queue"),
                        )?;
                        let cpu =
                            checked_usize(binding.cpu, &format!("workers.bindings[{index}].cpu"))?;
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
            }?;
            Ok(policy)
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
            let mut seen = BTreeSet::new();
            let mut queues = Vec::with_capacity(values.len());
            for (index, value) in values.into_iter().enumerate() {
                let toml::Value::Integer(value) = value else {
                    return Err(format!(
                        "tx.queues array values must be integers; found {}",
                        value.type_str()
                    ));
                };
                let queue = checked_queue(value, &format!("tx.queues[{index}]"))?;
                if !seen.insert(queue) {
                    return Err(format!("tx.queues contains duplicate queue {queue}"));
                }
                queues.push(queue);
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
        None => 1,
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

fn validate_user_interfaces(base: &toml::Value, user: &toml::Value) -> Result<(), String> {
    let Some(interfaces) = user.get("interfaces").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    let inherited = base.get("interfaces").and_then(toml::Value::as_table);
    for (label, interface) in interfaces {
        let path = interface_path(label);
        let Some(interface) = interface.as_table() else {
            continue;
        };
        // Atomic choices are validated per layer with the same parsers the merged
        // config uses, so a conflict inside one file is reported against that file
        // rather than surfacing as a merged-value error.
        if let Some(device) = interface.get("device") {
            device
                .clone()
                .try_into::<DeviceSelector>()
                .map_err(|error| format!("{path}.device is invalid: {error}"))?;
        }
        let xdp = interface.get("xdp").and_then(toml::Value::as_table);
        if let Some(workers) = xdp.and_then(|xdp| xdp.get("workers")) {
            workers
                .clone()
                .try_into::<WorkerPolicy>()
                .map_err(|error| format!("{path}.xdp.workers is invalid: {error}"))?;
        }
        if inherited.is_some_and(|interfaces| interfaces.contains_key(label)) {
            continue;
        }
        let mut missing = Vec::new();
        if !interface.contains_key("device") {
            missing.push(format!("{path}.device"));
        }
        if !xdp.is_some_and(|xdp| xdp.contains_key("zero_copy")) {
            missing.push(format!("{path}.xdp.zero_copy"));
        }
        if !xdp.is_some_and(|xdp| xdp.contains_key("workers")) {
            missing.push(format!("{path}.xdp.workers"));
        }
        if !missing.is_empty() {
            return Err(format!(
                "new interface {label:?} is incomplete; missing {}",
                missing.join(", ")
            ));
        }
    }
    Ok(())
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
        if user
            .get(name)
            .and_then(|module| module.get("xdp"))
            .and_then(|xdp| xdp.get("tx"))
            .is_some_and(|tx| tx.get("interface").is_some())
        {
            module.tx.interface_source = Source::User;
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
            validate_user_interfaces(&built_in, &user)?;
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
    if let Some(name) = &overrides.interface {
        validate_device_name(name, "--xdp-interface")?;
    }
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
                    module.enabled
                        && module.tx.queues_source == Source::User
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

/// Queue ids a module transmits over, in its own sender order. Module gating is
/// the caller's business: unused-worker diagnostics ask this of disabled modules
/// too.
fn module_queue_ids(module: &ModuleXdp, pool: &[u32]) -> Vec<u32> {
    match &module.tx.queues {
        QueueSelection::All => pool.to_vec(),
        QueueSelection::Explicit(queues) => queues.clone(),
    }
}

fn worker_queue_ids(policy: &WorkerPolicy) -> Vec<u32> {
    match policy {
        WorkerPolicy::Auto { count } => (0..*count as u32).collect(),
        WorkerPolicy::Cpus(cpus) => (0..cpus.len() as u32).collect(),
        WorkerPolicy::Bindings(bindings) => bindings.iter().map(|binding| binding.queue).collect(),
    }
}

/// Validate host-independent cross-references. Only live module bindings are
/// fatal; dormant-policy problems are reported as warnings.
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
    let used: BTreeSet<&str> = config
        .named_modules()
        .into_iter()
        .filter(|(_, module)| module.enabled)
        .map(|(_, module)| module.tx.interface.as_str())
        .collect();
    if used.len() > 1 {
        let names: Vec<_> = used.iter().map(|name| format!("{name:?}")).collect();
        return Err(format!(
            "XDP version 1 supports one interface, but enabled modules use {}; point every \
             module's tx.interface at the same label",
            names.join(", ")
        ));
    }
    for (name, module) in config.named_modules() {
        if module.tx.interface == *label {
            continue;
        }
        let message = format!(
            "{name}.xdp.tx.interface names {:?}, which is not a declared interface; declared: \
             {:?}",
            module.tx.interface, label
        );
        if active && module.enabled {
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
                if config.xdp.enabled && module.enabled {
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
        .map(|(name, module)| (name, module.enabled, module_queue_ids(module, &pool)))
        .collect();
    for queue in &pool {
        if selections
            .iter()
            .any(|(_, enabled, queues)| *enabled && queues.contains(queue))
        {
            continue;
        }
        let disabled_refs: Vec<_> = selections
            .iter()
            .filter(|(_, enabled, queues)| !*enabled && queues.contains(queue))
            .map(|(name, _, _)| *name)
            .collect();
        let reason = if disabled_refs.is_empty() {
            "unreferenced"
        } else {
            "disabled-modules-only"
        };
        let message = format!(
            "{} worker queue {queue} on interface {label:?} is inactive ({reason}){}",
            match interface.xdp.workers_source {
                Source::BuiltIn => "built-in",
                Source::User => "user-authored",
                Source::Cli => "CLI-authored",
            },
            if disabled_refs.is_empty() {
                String::new()
            } else {
                format!(
                    "; referenced by disabled module(s) {}",
                    disabled_refs.join(", ")
                )
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
    let selected = |module: &ModuleXdp| -> Vec<u32> {
        if !module.enabled {
            return Vec::new();
        }
        module_queue_ids(module, &pool)
    };
    let selected = Modules {
        gossip: selected(&config.gossip.xdp),
        repair: selected(&config.repair.xdp),
        tpu: selected(&config.tpu.xdp),
        turbine: selected(&config.turbine.xdp),
    };
    let active_ids: BTreeSet<_> = selected.values().into_iter().flatten().copied().collect();
    let active_workers: Vec<_> = declared
        .into_iter()
        .filter(|binding| active_ids.contains(&binding.queue))
        .collect();
    let active_cpus: BTreeSet<_> = active_workers.iter().map(|binding| binding.cpu).collect();
    if !active_cpus.is_empty() && active_cpus.len() == allowed_cpus.len() {
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
    let module_positions = |name: &str, module: &ModuleXdp, queues: &[u32]| -> Result<_, String> {
        if !module.enabled {
            return Ok(None);
        }
        let positions = queues
            .iter()
            .map(|queue| {
                positions.get(queue).copied().ok_or_else(|| {
                    format!(
                        "internal XDP resolution error: module {name} selected undeclared queue \
                         {queue}"
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if positions.is_empty() {
            return Err(format!(
                "internal XDP resolution error: module {name} selected no queues"
            ));
        }
        Ok(Some(positions.into_boxed_slice()))
    };
    if active_workers.is_empty() {
        return Err("active XDP policy selected no workers".to_string());
    }
    let modules = Modules {
        gossip: module_positions("gossip", &config.gossip.xdp, &selected.gossip)?,
        repair: module_positions("repair", &config.repair.xdp, &selected.repair)?,
        tpu: module_positions("tpu", &config.tpu.xdp, &selected.tpu)?,
        turbine: module_positions("turbine", &config.turbine.xdp, &selected.turbine)?,
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
[tpu.xdp]
tx.queues = [0]
[turbine.xdp]
tx.queues = [0]
[repair.xdp]
tx.queues = [0]
[gossip.xdp]
tx.queues = [0]
"#;

    fn user(contents: &str) -> EffectiveConfig {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        load(Some(file.path())).unwrap()
    }

    fn user_with_all_modules_queue_zero(contents: &str) -> EffectiveConfig {
        user(&format!("{contents}{ALL_MODULES_QUEUE_ZERO}"))
    }

    #[test]
    fn embedded_default_is_complete() {
        let config = load(None).unwrap();
        assert!(config.xdp.enabled);
        assert_eq!(config.interfaces.len(), 1);
        let interface = &config.interfaces["primary"];
        assert_eq!(interface.device, DeviceSelector::DefaultRoute);
        assert_eq!(interface.xdp.workers, WorkerPolicy::Auto { count: 1 });
        assert!(
            config
                .named_modules()
                .into_iter()
                .all(|(_, module)| module.enabled)
        );
    }

    #[test]
    fn embedded_default_resolves_from_policy_without_fallbacks() {
        let config = load(None).unwrap();
        let allowed = BTreeSet::from([1, 3, 5]);
        let (runtime, warnings) = resolve_runtime(&config, &allowed, Some(3)).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            runtime.queues.clone(),
            vec![QueueCpuBinding { queue: 0, cpu: 5 }]
        );
        for module in runtime.modules.values() {
            assert_eq!(module.as_deref(), Some(&[0][..]));
        }
    }

    #[test]
    fn scalar_patch_inherits_atomic_choices() {
        let config = user(
            r#"
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
    fn new_interface_error_lists_missing_fields() {
        let error = user_error(
            r#"
[interfaces.fast.xdp]
zero_copy = false
"#,
        );
        assert!(
            error.contains("new interface \"fast\" is incomplete"),
            "{error}"
        );
        assert!(error.contains("interfaces.fast.device"), "{error}");
        assert!(error.contains("interfaces.fast.xdp.workers"), "{error}");
    }

    #[test]
    fn workers_replace_atomically() {
        let config = user(
            r#"
[interfaces.primary.xdp]
workers.cpus = [8, 9]
"#,
        );
        assert_eq!(
            config.interfaces["primary"].xdp.workers,
            WorkerPolicy::Cpus(vec![8, 9])
        );

        let error = user_error(
            r#"
[interfaces.primary.xdp]
workers.unused = "warn"
"#,
        );
        assert!(error.contains("unknown field `unused`"), "{error}");
    }

    #[test]
    fn selector_conflicts_are_rejected_before_merge_or_completeness_checks() {
        for (case, contents, expected) in [
            (
                "existing interface device",
                r#"
[interfaces.primary]
device.route = "default"
device.name = "eth0"
"#,
                "conflicting keys device.route and device.name",
            ),
            (
                "new interface device",
                r#"
[interfaces.fast]
device.route = "default"
device.name = "eth0"
"#,
                "conflicting keys device.route and device.name",
            ),
            (
                "existing interface workers",
                r#"
[interfaces.primary.xdp]
workers.auto.count = 1
workers.cpus = [8]
"#,
                "conflicting worker modes",
            ),
            (
                "new interface workers",
                r#"
[interfaces.fast.xdp]
workers.auto.count = 1
workers.cpus = [8]
"#,
                "conflicting worker modes",
            ),
        ] {
            let error = user_error(contents);
            assert!(error.contains(expected), "{case}: {error}");
        }
    }

    #[test]
    fn device_names_follow_linux_syntax() {
        for name in ["eth0", "ens1f0.100", "veth@if", "123456789012345"] {
            validate_device_name(name, "device.name").unwrap();
        }
        for name in [
            "",
            ".",
            "..",
            "bad/name",
            "bad:name",
            "bad name",
            "bad\tname",
            "bad\nname",
            "bad\0name",
            "1234567890123456",
        ] {
            let error = validate_device_name(name, "device.name").unwrap_err();
            assert!(
                error.contains("not a valid platform interface name"),
                "{error}"
            );
        }
    }

    #[test]
    fn bindings_require_named_device_before_cli() {
        let error = user_error(
            r#"
[interfaces.primary.xdp]
workers.bindings = [{ queue = 0, cpu = 8 }]
"#,
        );
        assert!(
            error.contains("workers.bindings requires device.name"),
            "{error}"
        );
    }

    #[test]
    fn absent_version_is_one_and_mismatch_fails() {
        user(
            r#"
[xdp]
enabled = false
"#,
        );
        let error = user_error(
            r#"
schema_version = 2
"#,
        );
        assert!(error.contains("supports version 1"), "{error}");
    }

    #[test]
    fn dormant_queue_error_warns() {
        let config = user(
            r#"
[xdp]
enabled = false
[tpu.xdp]
tx.queues = [1]
"#,
        );
        assert!(!validate_policy(&config).unwrap().is_empty());
    }

    #[test]
    fn every_worker_mode_enforces_the_same_cardinality_limit() {
        let too_many = MAX_XDP_WORKERS + 1;
        let cpus: Vec<_> = (0..too_many).collect();
        let bindings = (0..too_many)
            .map(|value| format!("{{ queue = {value}, cpu = {value} }}"))
            .collect::<Vec<_>>()
            .join(", ");
        for (mode, contents) in [
            (
                "auto",
                format!(
                    r#"
[interfaces.primary.xdp]
workers.auto.count = {too_many}
"#
                ),
            ),
            (
                "cpus",
                format!(
                    r#"
[interfaces.primary.xdp]
workers.cpus = {cpus:?}
"#
                ),
            ),
            (
                "bindings",
                format!(
                    r#"
[interfaces.primary.xdp]
workers.bindings = [{bindings}]
"#
                ),
            ),
        ] {
            let error = user_error(&contents);
            assert!(error.contains("4096"), "{mode}: {error}");
        }
    }

    #[test]
    fn renaming_the_interface_requires_updating_module_references() {
        const RENAMED: &str = r#"
[interfaces.fast]
device.name = "eth0"
[interfaces.fast.xdp]
zero_copy = false
workers.cpus = [8]
"#;
        let error = validate_policy(&user(RENAMED)).unwrap_err();
        assert!(error.contains("not a declared interface"), "{error}");

        let pointed = user(&format!(
            r#"{RENAMED}
[tpu.xdp]
tx.interface = "fast"
[turbine.xdp]
tx.interface = "fast"
[repair.xdp]
tx.interface = "fast"
[gossip.xdp]
tx.interface = "fast"
"#
        ));
        let (runtime, _) = resolve_runtime(&pointed, &BTreeSet::from([8, 9]), None).unwrap();
        assert_eq!(runtime.interface_label, "fast");
        assert!(
            runtime
                .modules
                .values()
                .iter()
                .all(|module| module.is_some())
        );
    }

    #[test]
    fn using_more_than_one_interface_is_rejected() {
        let config = user(
            r#"
[tpu.xdp]
tx.interface = "other"
"#,
        );
        let error = validate_policy(&config).unwrap_err();
        assert!(error.contains("supports one interface"), "{error}");
    }

    #[test]
    fn cli_worker_replacement_rejects_user_queue_ids_even_if_they_survive() {
        let config = user(
            r#"
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
    fn cli_worker_replacement_ignores_disabled_module_queue_ids() {
        let config = user(
            r#"
[interfaces.primary.xdp]
workers.cpus = [8, 9]
[tpu.xdp]
enabled = false
tx.queues = [0]
"#,
        );
        let application = apply_cli(
            config,
            CliOverrides {
                cpu_cores: Some(vec![10, 11]),
                ..CliOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(
            application.config.interfaces["primary"].xdp.workers,
            WorkerPolicy::Cpus(vec![10, 11])
        );
    }

    #[test]
    fn cli_cpu_workers_preserve_module_queue_scoping() {
        let config = user(
            r#"
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
        assert_eq!(runtime.modules.tpu.as_deref(), Some(&[0][..]));
        assert_eq!(runtime.modules.turbine.as_deref(), Some(&[0, 1][..]));
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
    fn every_worker_mode_must_leave_a_cpu_unreserved() {
        for (mode, contents) in [
            (
                "auto",
                r#"
[interfaces.primary.xdp]
workers.auto.count = 2
"#,
            ),
            (
                "cpus",
                r#"
[interfaces.primary.xdp]
workers.cpus = [8, 9]
"#,
            ),
            (
                "bindings",
                r#"[interfaces.primary]
device.name = "eth0"
[interfaces.primary.xdp]
workers.bindings = [{ queue = 0, cpu = 8 }, { queue = 1, cpu = 9 }]
"#,
            ),
        ] {
            let config = user(contents);
            let error = resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap_err();
            assert!(error.contains("leave at least one"), "{mode}: {error}");
        }
    }

    #[test]
    fn unselected_workers_do_not_reserve_their_cpus() {
        let config = user_with_all_modules_queue_zero(
            r#"
[interfaces.primary.xdp]
workers.cpus = [8, 9]
"#,
        );
        let (runtime, _) = resolve_runtime(&config, &BTreeSet::from([8, 9]), None).unwrap();
        assert_eq!(runtime.queues, [QueueCpuBinding { queue: 0, cpu: 8 }]);
    }

    #[test]
    fn inactive_cli_overrides_are_ignored_without_topology_validation() {
        let config = user(
            r#"
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
        let application = apply_cli(
            config,
            CliOverrides {
                interface: Some("eth1".to_string()),
                ..CliOverrides::default()
            },
        )
        .unwrap();
        assert!(
            application
                .warnings
                .iter()
                .any(|warning| warning.contains("ignoring --xdp-interface=eth1"))
        );
        assert_eq!(application.config.interfaces.len(), 2);
        assert!(validate_policy(&application.config).is_ok());
    }

    #[test]
    fn unreferenced_workers_warn_for_every_source() {
        let mut built_in_workers = user(ALL_MODULES_QUEUE_ZERO);
        built_in_workers
            .interfaces
            .get_mut("primary")
            .unwrap()
            .xdp
            .workers = WorkerPolicy::Cpus(vec![8, 9]);
        let user_workers = user_with_all_modules_queue_zero(
            r#"
[interfaces.primary.xdp]
workers.cpus = [8, 9]
"#,
        );
        let cli_workers = apply_cli(
            user(ALL_MODULES_QUEUE_ZERO),
            CliOverrides {
                cpu_cores: Some(vec![8, 9]),
                ..CliOverrides::default()
            },
        )
        .unwrap()
        .config;
        for (source, config, expected) in [
            ("built-in", built_in_workers, "built-in worker queue 1"),
            ("user", user_workers, "user-authored worker queue 1"),
            ("CLI", cli_workers, "CLI-authored worker queue 1"),
        ] {
            let warnings = validate_policy(&config).unwrap();
            assert!(
                warnings.iter().any(|warning| warning.contains(expected)),
                "{source}: {warnings:?}"
            );
        }
    }

    #[test]
    fn queue_selection_type_error_is_targeted() {
        let error = user_error(
            r#"
[tpu.xdp]
tx.queues = true
"#,
        );
        assert!(
            error.contains("accepts only \"all\" or a non-empty integer array"),
            "{error}"
        );
    }

    fn user_error(contents: &str) -> String {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        load(Some(file.path())).unwrap_err()
    }
}
