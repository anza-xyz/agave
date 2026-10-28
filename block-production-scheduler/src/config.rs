use {
    agave_scheduler_handshake::ClientLogon,
    core::time::Duration,
    serde::{Deserialize, Deserializer},
    std::path::PathBuf,
};

const DEFAULT_CONFIG_TOML: &str = include_str!("../default.toml");

/// Configuration for connecting a scheduler through a handshake socket.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Path to Agave's scheduler handshake socket; required in the user's TOML file.
    pub ipc_path: PathBuf,
    /// Log file for the standalone binary. Omit to log to stderr.
    pub log_file: Option<PathBuf>,
    pub session: SessionConfig,
    pub scheduler: SchedulerConfig,
}

impl Config {
    /// Applies TOML overrides to the embedded defaults. `ipc_path` has no default.
    pub fn from_toml(source: &str) -> Result<Self, toml::de::Error> {
        let mut values = defaults();
        merge(&mut values, toml::from_str(source)?);
        values.try_into()
    }
}

/// Session settings shared by local and external schedulers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Timeout for handshake reads and writes.
    #[serde(rename = "handshake_timeout_ms", deserialize_with = "duration_millis")]
    pub handshake_timeout: Duration,
    /// Number of execution workers requested from Agave.
    pub worker_count: usize,
    /// Number of check workers requested from Agave.
    pub check_worker_count: usize,
    /// Minimum shared allocator size in bytes.
    pub allocator_size: usize,
    /// Number of allocator handles requested by this scheduler.
    pub allocator_handles: usize,
    /// Minimum TPU-to-scheduler queue capacity in messages.
    pub tpu_to_pack_capacity: usize,
    /// Minimum progress queue capacity in messages.
    pub progress_tracker_capacity: usize,
    /// Minimum scheduler-to-execution-worker queue capacity in messages.
    pub pack_to_worker_capacity: usize,
    /// Minimum execution-worker-to-scheduler queue capacity in messages.
    pub worker_to_pack_capacity: usize,
    /// Minimum scheduler-to-check-worker queue capacity in messages.
    pub pack_to_check_worker_capacity: usize,
    /// Minimum check-worker-to-scheduler queue capacity in messages.
    pub check_worker_to_pack_capacity: usize,
}

impl SessionConfig {
    pub fn client_logon(&self) -> ClientLogon {
        ClientLogon {
            worker_count: self.worker_count,
            check_worker_count: self.check_worker_count,
            simulate_worker_count: 0,
            allocator_size: self.allocator_size,
            allocator_handles: self.allocator_handles,
            tpu_to_pack_capacity: self.tpu_to_pack_capacity,
            progress_tracker_capacity: self.progress_tracker_capacity,
            pack_to_worker_capacity: self.pack_to_worker_capacity,
            worker_to_pack_capacity: self.worker_to_pack_capacity,
            pack_to_check_worker_capacity: self.pack_to_check_worker_capacity,
            check_worker_to_pack_capacity: self.check_worker_to_pack_capacity,
            pack_to_simulate_worker_capacity: 0,
            simulate_worker_to_pack_capacity: 0,
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        defaults()
            .remove("session")
            .expect("embedded session defaults exist")
            .try_into()
            .expect("embedded session defaults are valid")
    }
}

/// Scheduler behavior independent of how its session is established.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerConfig {
    /// Maximum number of checked transactions retained for scheduling.
    pub transaction_state_capacity: usize,
    /// Time before slot end by which pacing releases the full cost budget.
    #[serde(rename = "execution_margin_ms", deserialize_with = "duration_millis")]
    pub execution_margin: Duration,
    /// Outstanding estimated CU target per worker, including pending batches.
    /// The last assigned transaction may cross this target.
    pub max_cost_units_per_worker: u64,
    /// Estimated CU target per execution batch, checked after adding each transaction.
    pub max_cost_units_per_batch: u64,
    /// Target serialized entry bytes per execution batch, including entry overhead.
    pub target_entry_bytes_per_batch: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        defaults()
            .remove("scheduler")
            .expect("embedded scheduler defaults exist")
            .try_into()
            .expect("embedded scheduler defaults are valid")
    }
}

fn defaults() -> toml::Table {
    toml::from_str(DEFAULT_CONFIG_TOML).expect("embedded defaults are valid TOML")
}

fn merge(values: &mut toml::Table, overrides: toml::Table) {
    for (key, value) in overrides {
        match (values.get_mut(&key), value) {
            (Some(toml::Value::Table(values)), toml::Value::Table(overrides)) => {
                merge(values, overrides);
            }
            (_, value) => {
                values.insert(key, value);
            }
        }
    }
}

fn duration_millis<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    u64::deserialize(deserializer).map(Duration::from_millis)
}
