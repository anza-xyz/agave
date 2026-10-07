//! Per-link network delay and jitter for local clusters.
//!
//! Delay is applied with `tc netem` on the loopback device, which needs `CAP_NET_ADMIN`.
//! To keep that from ever affecting the host, the controller only acts when the process
//! runs inside the dedicated network namespace named by [`NETNS_ENV`], as set up by
//! `local-cluster/run-in-netns.sh`, and runs `tc` there through passwordless `sudo`.
//!
//! Each validator must bind to its own loopback address, which
//! `ClusterConfig::enable_network_partitions` provides. Every directed validator pair
//! gets its own netem class, selected by source and destination address. Jitter larger
//! than the gap between packets reorders them.

use {
    crate::local_cluster::LocalCluster,
    log::*,
    solana_pubkey::Pubkey,
    std::{
        collections::{BTreeMap, HashSet},
        io::{Error, ErrorKind, Result, Write},
        net::IpAddr,
        process::{Command, Stdio},
        time::Duration,
    },
};

/// Names the network namespace the controller is allowed to modify.
pub const NETNS_ENV: &str = "LOCAL_CLUSTER_NETNS";

/// The netem queue limit, in packets. The default of 1000 drops packets under delay.
const NETEM_LIMIT: u32 = 100_000;

/// The delay applied to packets on one directed link.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkDelay {
    /// The mean added latency.
    pub delay: Duration,
    /// Each packet's latency is drawn uniformly from `delay ± jitter`.
    pub jitter: Duration,
}

/// Dynamically delays traffic between pairs of validators.
pub struct NetworkDelayController {
    /// The tc class id serving each directed (sender, receiver) link.
    links: BTreeMap<(Pubkey, Pubkey), u32>,
}

impl NetworkDelayController {
    /// Installs one netem class per directed validator pair, with no delay.
    ///
    /// Returns `Ok(None)` when the process is not inside the namespace named by
    /// [`NETNS_ENV`], so callers can skip delay faults rather than touch the host.
    pub fn new(cluster: &LocalCluster) -> Result<Option<Self>> {
        let Some(netns) = std::env::var_os(NETNS_ENV) else {
            warn!("{NETNS_ENV} is not set; network delay faults are disabled");
            return Ok(None);
        };
        if !in_netns(&netns.to_string_lossy())? {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!("{NETNS_ENV}={netns:?} does not name this process's network namespace"),
            ));
        }

        let ips = cluster
            .validators
            .iter()
            .map(|(pubkey, validator)| {
                let ip = validator.info.contact_info.gossip().map(|addr| addr.ip());
                ip.map(|ip| (*pubkey, ip)).ok_or_else(|| {
                    Error::new(ErrorKind::NotFound, format!("no address for {pubkey}"))
                })
            })
            .collect::<Result<BTreeMap<Pubkey, IpAddr>>>()?;
        if ips.values().collect::<HashSet<_>>().len() != ips.len() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "network delay needs a distinct address per validator; set \
                 ClusterConfig::enable_network_partitions",
            ));
        }

        // Class 1:1 carries all unmatched traffic undelayed.
        let mut commands = vec![
            "qdisc replace dev lo root handle 1: htb default 1".to_string(),
            "class add dev lo parent 1: classid 1:1 htb rate 10gbit quantum 60000".to_string(),
        ];
        let mut links = BTreeMap::new();
        for (from, from_ip) in &ips {
            for (to, to_ip) in &ips {
                if from == to {
                    continue;
                }
                let class = u32::try_from(links.len()).unwrap() + 2;
                commands.push(format!(
                    "class add dev lo parent 1: classid 1:{class:x} htb rate 10gbit quantum 60000"
                ));
                commands.push(format!(
                    "qdisc add dev lo parent 1:{class:x} handle {class:x}: netem delay 0ms limit \
                     {NETEM_LIMIT}"
                ));
                commands.push(format!(
                    "filter add dev lo parent 1: protocol ip prio 1 u32 match ip src {from_ip}/32 \
                     match ip dst {to_ip}/32 flowid 1:{class:x}"
                ));
                links.insert((*from, *to), class);
            }
        }
        run_tc_batch(&commands)?;
        Ok(Some(Self { links }))
    }

    /// Sets the delay of every listed link and removes it from all others. Links must
    /// be between distinct validators of the cluster.
    pub fn apply(&self, delays: &BTreeMap<(Pubkey, Pubkey), LinkDelay>) -> Result<()> {
        if let Some(link) = delays.keys().find(|link| !self.links.contains_key(link)) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("unknown link {link:?}"),
            ));
        }
        let commands = self
            .links
            .iter()
            .map(|(link, class)| {
                let LinkDelay { delay, jitter } = delays.get(link).copied().unwrap_or_default();
                let jitter = if jitter.is_zero() {
                    String::new()
                } else {
                    format!(" {}us", jitter.as_micros())
                };
                format!(
                    "qdisc change dev lo parent 1:{class:x} handle {class:x}: netem delay \
                     {}us{jitter} limit {NETEM_LIMIT}",
                    delay.as_micros()
                )
            })
            .collect::<Vec<_>>();
        run_tc_batch(&commands)
    }

    /// Removes all delay.
    pub fn heal(&self) -> Result<()> {
        self.apply(&BTreeMap::new())
    }
}

/// Whether this process is in the network namespace that `ip netns` calls `name`.
fn in_netns(name: &str) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let named = std::fs::metadata(format!("/run/netns/{name}"))?;
    let current = std::fs::metadata("/proc/self/ns/net")?;
    Ok(named.dev() == current.dev() && named.ino() == current.ino())
}

fn run_tc_batch(commands: &[String]) -> Result<()> {
    let mut child = Command::new("sudo")
        .args(["-n", "tc", "-force", "-batch", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.join("\n").as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::other(format!(
            "tc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}
