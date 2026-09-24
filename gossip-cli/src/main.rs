//! A command-line executable for monitoring a cluster's gossip plane.
#[allow(deprecated)]
use solana_gossip::{
    contact_info::ContactInfo,
    gossip_service::{discover_peers, make_node},
};
use {
    clap::{
        App, AppSettings, Arg, ArgMatches, SubCommand, crate_description, crate_name, value_t,
        value_t_or_exit, values_t,
    },
    log::{info, warn},
    solana_clap_utils::{
        hidden_unless_forced,
        input_parsers::{keypair_of, pubkeys_of},
        input_validators::{is_keypair_or_ask_keyword, is_port, is_pubkey},
    },
    solana_gossip::{
        cluster_info::ClusterInfo, crds_gossip_pull::CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS,
    },
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    std::{
        cmp,
        collections::HashMap,
        error,
        fs::File,
        io::BufReader,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        process::exit,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
};

/// Default gossip liveness sampling cadence, matching the one-second loop that
/// `wait_for_supermajority` runs in the validator.
const DEFAULT_WFSM_INTERVAL_MS: u64 = 1000;
// Mirrors the private constant of the same name in core/src/validator.rs.
const NODE_LIVENESS_TIMEOUT_MS: u64 = 3 * CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS;
const WFSM_LOG_TAG: &str = "RRRRRRRRRR";

fn get_clap_app<'ab, 'v>(name: &str, about: &'ab str, version: &'v str) -> App<'ab, 'v> {
    let shred_version_arg = Arg::with_name("shred_version")
        .long("shred-version")
        .value_name("VERSION")
        .takes_value(true)
        .default_value("0")
        .help("Filter gossip nodes by this shred version");

    let gossip_port_arg = clap::Arg::with_name("gossip_port")
        .long("gossip-port")
        .value_name("PORT")
        .takes_value(true)
        .validator(is_port)
        .help("Gossip port number for the node");

    let bind_address_arg = clap::Arg::with_name("bind_address")
        .long("bind-address")
        .value_name("HOST")
        .takes_value(true)
        .validator(solana_net_utils::is_host)
        .help("IP address to bind the node to for gossip");

    App::new(name)
        .about(about)
        .version(version)
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .arg(
            Arg::with_name("allow_private_addr")
                .long("allow-private-addr")
                .takes_value(false)
                .help("Allow contacting private ip addresses")
                .hidden(hidden_unless_forced()),
        )
        .subcommand(
            SubCommand::with_name("rpc-url")
                .about("Get an RPC URL for the cluster")
                .arg(
                    Arg::with_name("entrypoint")
                        .short("n")
                        .long("entrypoint")
                        .value_name("HOST:PORT")
                        .takes_value(true)
                        .required(true)
                        .validator(solana_net_utils::is_host_port)
                        .help("Rendezvous with the cluster at this entry point"),
                )
                .arg(
                    Arg::with_name("all")
                        .long("all")
                        .takes_value(false)
                        .help("Return all RPC URLs"),
                )
                .arg(
                    Arg::with_name("any")
                        .long("any")
                        .takes_value(false)
                        .conflicts_with("all")
                        .help("Return any RPC URL"),
                )
                .arg(
                    Arg::with_name("timeout")
                        .long("timeout")
                        .value_name("SECONDS")
                        .takes_value(true)
                        .default_value("15")
                        .help("Timeout in seconds"),
                )
                .arg(&shred_version_arg)
                .arg(&gossip_port_arg)
                .arg(&bind_address_arg)
                .setting(AppSettings::DisableVersion),
        )
        .subcommand(
            SubCommand::with_name("spy")
                .about("Monitor the gossip entrypoint")
                .setting(AppSettings::DisableVersion)
                .arg(
                    Arg::with_name("entrypoint")
                        .short("n")
                        .long("entrypoint")
                        .value_name("HOST:PORT")
                        .takes_value(true)
                        .multiple(true)
                        .validator(solana_net_utils::is_host_port)
                        .help("Rendezvous with the cluster at this entrypoint"),
                )
                .arg(
                    Arg::with_name("identity")
                        .short("i")
                        .long("identity")
                        .value_name("PATH")
                        .takes_value(true)
                        .validator(is_keypair_or_ask_keyword)
                        .help("Identity keypair [default: ephemeral keypair]"),
                )
                .arg(
                    Arg::with_name("num_nodes")
                        .short("N")
                        .long("num-nodes")
                        .value_name("NUM")
                        .takes_value(true)
                        .conflicts_with("num_nodes_exactly")
                        .help("Wait for at least NUM nodes to be visible"),
                )
                .arg(
                    Arg::with_name("num_nodes_exactly")
                        .short("E")
                        .long("num-nodes-exactly")
                        .value_name("NUM")
                        .takes_value(true)
                        .help("Wait for exactly NUM nodes to be visible"),
                )
                .arg(
                    Arg::with_name("node_pubkey")
                        .short("p")
                        .long("pubkey")
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .validator(is_pubkey)
                        .multiple(true)
                        .help("Public key of a specific node to wait for"),
                )
                .arg(&shred_version_arg)
                .arg(&gossip_port_arg)
                .arg(&bind_address_arg)
                .arg(
                    Arg::with_name("timeout")
                        .long("timeout")
                        .value_name("SECONDS")
                        .takes_value(true)
                        .help("Maximum time to wait in seconds [default: wait forever]"),
                )
                .arg(
                    Arg::with_name("wfsm_stakes")
                        .long("wfsm-stakes")
                        .value_name("PATH")
                        .takes_value(true)
                        .help(
                            "Run indefinitely, logging wait-for-supermajority gossip liveness \
                             samples against the getVoteAccounts snapshot at PATH. The snapshot \
                             is re-read whenever it changes on disk",
                        ),
                )
                .arg(
                    Arg::with_name("wfsm_interval_ms")
                        .long("wfsm-interval-ms")
                        .value_name("MILLIS")
                        .takes_value(true)
                        .requires("wfsm_stakes")
                        .help("Interval between liveness samples [default: 1000]"),
                )
                .arg(
                    Arg::with_name("wfsm_include_unstaked")
                        .long("wfsm-include-unstaked")
                        .takes_value(false)
                        .requires("wfsm_stakes")
                        .help(
                            "Also emit grace-band entries for nodes carrying no stake, reported \
                             at 0.000%",
                        ),
                ),
        )
}

fn parse_matches() -> ArgMatches<'static> {
    get_clap_app(
        crate_name!(),
        crate_description!(),
        solana_version::version!(),
    )
    .get_matches()
}

/// Determine bind address by checking these sources in order:
/// 1. --bind-address cli arg
/// 2. connect to entrypoints to determine my public IP address
fn parse_bind_address(matches: &ArgMatches, entrypoint_addrs: &[SocketAddr]) -> IpAddr {
    if let Some(bind_address) = matches.value_of("bind_address") {
        solana_net_utils::parse_host(bind_address).unwrap_or_else(|e| {
            eprintln!("failed to parse bind-address: {e}");
            exit(1);
        })
    } else if let Some(bind_addr) = get_bind_address_from_entrypoints(entrypoint_addrs) {
        bind_addr
    } else {
        eprintln!(
            "Failed to find a valid bind address. Bind address can be provided directly with \
             --bind-address or by the entrypoint functioning as an ip echo server."
        );
        exit(1);
    }
}

/// Find my public IP address by attempting connections to entrypoints until one succeeds.
fn get_bind_address_from_entrypoints(entrypoint_addrs: &[SocketAddr]) -> Option<IpAddr> {
    entrypoint_addrs.iter().find_map(|entrypoint_addr| {
        solana_net_utils::get_public_ip_addr_with_binding(
            entrypoint_addr,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        )
        .ok()
    })
}

// allow deprecations here to workaround limitations with dependency specification in
// multi-target crates and agave-unstable-api. `ContactInfo` is deprecated here, but we
// cannot specify deprecation allowances on function arguments. since this function is
// private, we apply the allowance to the entire body as a refactor that would limit it
// to a wrapper is going to be too invasive
//
// this mitigation can be removed once the solana-gossip binary target is moved to its
// own crate and we can correctly depend on the solana-gossip lib crate with
// `agave-unstable-api` enabled
#[allow(deprecated)]
fn process_spy_results(
    timeout: Option<u64>,
    validators: Vec<ContactInfo>,
    num_nodes: Option<usize>,
    num_nodes_exactly: Option<usize>,
    pubkeys: Option<&[Pubkey]>,
) {
    if timeout.is_some() {
        if let Some(num) = num_nodes
            && validators.len() < num
        {
            let add = if num_nodes_exactly.is_some() {
                ""
            } else {
                " or more"
            };
            eprintln!("Error: Insufficient validators discovered.  Expecting {num}{add}",);
            exit(1);
        }
        if let Some(nodes) = pubkeys {
            for node in nodes {
                if !validators.iter().any(|x| {
                    #[allow(deprecated)]
                    let pubkey = x.pubkey();
                    pubkey == node
                }) {
                    eprintln!("Error: Could not find node {node:?}");
                    exit(1);
                }
            }
        }
    }
    if let Some(num_nodes_exactly) = num_nodes_exactly
        && validators.len() > num_nodes_exactly
    {
        eprintln!("Error: Extra nodes discovered.  Expecting exactly {num_nodes_exactly}");
        exit(1);
    }
}

/// Check entrypoints until one returns a valid non-zero shred version
fn get_entrypoint_shred_version(entrypoint_addrs: &[SocketAddr]) -> Option<u16> {
    entrypoint_addrs.iter().find_map(|entrypoint_addr| {
        match solana_net_utils::get_cluster_shred_version(entrypoint_addr) {
            Err(err) => {
                warn!("get_cluster_shred_version failed: {entrypoint_addr}, {err}");
                None
            }
            Ok(0) => {
                warn!("entrypoint {entrypoint_addr} returned shred-version zero");
                None
            }
            Ok(shred_version) => {
                info!("obtained shred-version {shred_version} from entrypoint: {entrypoint_addr}");
                Some(shred_version)
            }
        }
    })
}

fn process_spy(matches: &ArgMatches, socket_addr_space: SocketAddrSpace) -> std::io::Result<()> {
    let num_nodes_exactly = matches
        .value_of("num_nodes_exactly")
        .map(|num| num.to_string().parse().unwrap());
    let num_nodes = matches
        .value_of("num_nodes")
        .map(|num| num.to_string().parse().unwrap())
        .or(num_nodes_exactly);
    let timeout = matches
        .value_of("timeout")
        .map(|secs| secs.to_string().parse().unwrap());
    let pubkeys = pubkeys_of(matches, "node_pubkey");
    let identity_keypair = keypair_of(matches, "identity");
    let entrypoint_addrs = parse_entrypoints(matches);
    let gossip_addr = get_gossip_address(matches, &entrypoint_addrs);

    let mut shred_version = value_t_or_exit!(matches, "shred_version", u16);
    if shred_version == 0 {
        shred_version = get_entrypoint_shred_version(&entrypoint_addrs)
            .expect("need non-zero shred-version to join the cluster");
    }

    if let Some(stakes_path) = matches.value_of("wfsm_stakes") {
        let interval = Duration::from_millis(
            value_t!(matches, "wfsm_interval_ms", u64).unwrap_or(DEFAULT_WFSM_INTERVAL_MS),
        );
        return wfsm_spy(
            identity_keypair,
            &entrypoint_addrs,
            &gossip_addr,
            shred_version,
            socket_addr_space,
            stakes_path,
            interval,
            matches.is_present("wfsm_include_unstaked"),
        )
        .map_err(|err| std::io::Error::other(err.to_string()));
    }

    let discover_timeout = Duration::from_secs(timeout.unwrap_or(u64::MAX));
    #[allow(deprecated)]
    let (_all_peers, validators) = discover_peers(
        identity_keypair,
        &entrypoint_addrs,
        num_nodes,
        discover_timeout,
        pubkeys.as_deref(),
        &[],
        Some(&gossip_addr),
        shred_version,
        socket_addr_space,
    )?;

    process_spy_results(
        timeout,
        validators,
        num_nodes,
        num_nodes_exactly,
        pubkeys.as_deref(),
    );

    Ok(())
}

/// Activated stake by node identity, and the total it is a fraction of.
///
/// Reads a raw `getVoteAccounts` response, either the full JSON-RPC envelope as
/// curl leaves it or the bare result object. Delinquent validators are included:
/// a validator's stake stays in the bank's vote accounts whether or not it is
/// voting, and this has to reproduce what `wait_for_supermajority` sees.
fn load_wfsm_stakes(path: &str) -> Result<(HashMap<Pubkey, u64>, u64), Box<dyn error::Error>> {
    let json: serde_json::Value = serde_json::from_reader(BufReader::new(File::open(path)?))?;
    let result = if json.get("result").is_some() {
        &json["result"]
    } else {
        &json
    };
    let mut stakes = HashMap::<Pubkey, u64>::new();
    let mut total = 0u64;
    for kind in ["current", "delinquent"] {
        let accounts = result
            .get(kind)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("{path}: no `{kind}` array in getVoteAccounts response"))?;
        for account in accounts {
            let stake = account["activatedStake"].as_u64().unwrap_or_default();
            if stake == 0 {
                continue;
            }
            let node = account["nodePubkey"]
                .as_str()
                .and_then(|key| key.parse::<Pubkey>().ok())
                .ok_or_else(|| format!("{path}: vote account with no usable nodePubkey"))?;
            // A node identity can back several vote accounts; gossip sees one node.
            *stakes.entry(node).or_default() += stake;
            total = total.saturating_add(stake);
        }
    }
    if total == 0 {
        return Err(format!("{path}: no activated stake").into());
    }
    Ok((stakes, total))
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
        .as_millis() as u64
}

fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let last = sorted.len() - 1;
    sorted[((last as f64) * quantile).round() as usize]
}

/// Age distribution across one population of nodes, as `(age, local_age)` pairs.
///
/// Not part of the format `wfsm_grace.py` parses -- that one only ever carries
/// the nodes sitting in the grace band, which cannot say how the rest of the
/// population is distributed. Both clocks are reported: `age` is the
/// peer-authored wallclock, which delivery latency inflates, while `local_age`
/// is stamped by us on insert and so answers "when did we last hear anything"
/// without depending on the peer's clock.
fn log_age_summary(label: &str, samples: &[(u64, u64)]) {
    let mut ages: Vec<u64> = samples.iter().map(|&(age, _)| age).collect();
    let mut local_ages: Vec<u64> = samples.iter().map(|&(_, local_age)| local_age).collect();
    ages.sort_unstable();
    local_ages.sort_unstable();
    let over = |values: &[u64], limit: u64| values.iter().filter(|&&value| value >= limit).count();
    info!(
        "{WFSM_LOG_TAG} ages {label} n={} age p50={} p90={} p99={} max={} over15s={} over45s={} \
         local_age p50={} p90={} p99={} max={} over15s={} over45s={}",
        samples.len(),
        percentile(&ages, 0.50),
        percentile(&ages, 0.90),
        percentile(&ages, 0.99),
        ages.last().copied().unwrap_or_default(),
        over(&ages, CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS),
        over(&ages, NODE_LIVENESS_TIMEOUT_MS),
        percentile(&local_ages, 0.50),
        percentile(&local_ages, 0.90),
        percentile(&local_ages, 0.99),
        local_ages.last().copied().unwrap_or_default(),
        over(&local_ages, CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS),
        over(&local_ages, NODE_LIVENESS_TIMEOUT_MS),
    );
}

/// One `RRRRRRRRRR` liveness sample, byte-identical in shape to the one the
/// validator emits from `get_stake_percent_in_gossip`, so the same parsers read
/// both. The spy carries no bank, so stake comes from the snapshot instead of
/// `bank.vote_accounts()`; everything downstream of that is the same arithmetic.
///
/// With `include_unstaked`, nodes carrying no stake get grace-band entries too,
/// reported at 0.000% so the stake sums downstream are unaffected. Mainnet is
/// mostly unstaked nodes, which the stake-driven sample cannot see at all.
fn wfsm_sample(
    cluster_info: &ClusterInfo,
    stakes: &HashMap<Pubkey, u64>,
    total_stake: u64,
    include_unstaked: bool,
) {
    let now = timestamp_ms();
    #[allow(deprecated)]
    let peers: HashMap<_, _> = cluster_info
        .all_peers()
        .into_iter()
        .map(|(node, local_timestamp)| {
            let age = now.saturating_sub(node.wallclock());
            let local_age = now.saturating_sub(local_timestamp);
            (*node.pubkey(), (age, local_age, node.gossip()))
        })
        .collect();

    let mut online_stake = 0u64;
    // Stake that would have been considered online under the old, un-tripled
    // timeout, and under a timeout applied to our own insert clock instead of
    // the peer-authored wallclock.
    let mut online_stake_old_timeout = 0u64;
    let mut online_stake_local_ts = 0u64;
    let mut offline_stake = 0u64;
    // Nodes that are only online thanks to the extended timeout.
    let mut grace_nodes = vec![];

    for (identity, stake) in stakes {
        match peers.get(identity) {
            Some(&(age, local_age, gossip_addr)) if age < NODE_LIVENESS_TIMEOUT_MS => {
                online_stake += stake;
                if local_age < CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS {
                    online_stake_local_ts += stake;
                }
                if age < CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS {
                    online_stake_old_timeout += stake;
                } else {
                    grace_nodes.push((*stake, *identity, gossip_addr, age, local_age));
                }
            }
            // Absent from crds, or present but already past the new timeout.
            _ => offline_stake += stake,
        }
    }

    let percent = |stake: u64| (stake as f64 / total_stake as f64) * 100.;
    info!(
        "{WFSM_LOG_TAG} {:.3}% of active stake visible in gossip with \
         {NODE_LIVENESS_TIMEOUT_MS}ms timeout, {:.3}% with {CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS}ms \
         timeout, {:.3}% with {CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS}ms local-insert timeout",
        percent(online_stake),
        percent(online_stake_old_timeout),
        percent(online_stake_local_ts),
    );
    grace_nodes.sort_by_key(|node| cmp::Reverse(node.0)); // sort by reverse stake weight
    for (stake, identity, gossip_addr, age, local_age) in grace_nodes {
        info!(
            "{WFSM_LOG_TAG}    {:.3}% - {identity} - gossip {} - age {age}ms - local_age \
             {local_age}ms",
            percent(stake),
            gossip_addr.map_or_else(|| "none".to_string(), |addr| addr.to_string()),
        );
    }
    if include_unstaked {
        // Same window as the staked entries above -- 15s up to the new timeout --
        // so grace-band episodes mean the same thing in both populations.
        let mut unstaked_grace: Vec<_> = peers
            .iter()
            .filter(|(identity, _)| !stakes.contains_key(*identity))
            .filter(|&(_, &(age, _, _))| {
                (CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS..NODE_LIVENESS_TIMEOUT_MS).contains(&age)
            })
            .map(|(identity, &(age, local_age, gossip_addr))| {
                (*identity, gossip_addr, age, local_age)
            })
            .collect();
        unstaked_grace.sort_unstable_by_key(|node| cmp::Reverse(node.2)); // oldest first
        for (identity, gossip_addr, age, local_age) in unstaked_grace {
            info!(
                "{WFSM_LOG_TAG}    0.000% - {identity} - gossip {} - age {age}ms - local_age \
                 {local_age}ms",
                gossip_addr.map_or_else(|| "none".to_string(), |addr| addr.to_string()),
            );
        }
    }

    let (staked_ages, unstaked_ages): (Vec<_>, Vec<_>) = peers
        .iter()
        .map(|(identity, &(age, local_age, _))| (stakes.contains_key(identity), (age, local_age)))
        .partition(|(staked, _)| *staked);
    log_age_summary(
        "staked",
        &staked_ages
            .into_iter()
            .map(|(_, ages)| ages)
            .collect::<Vec<_>>(),
    );
    log_age_summary(
        "unstaked",
        &unstaked_ages
            .into_iter()
            .map(|(_, ages)| ages)
            .collect::<Vec<_>>(),
    );
    // Not part of the parsed format; keeps the offline mass visible in the log.
    info!(
        "{WFSM_LOG_TAG} offline {:.3}% of active stake, {} staked identities unseen",
        percent(offline_stake),
        stakes.len() - peers.keys().filter(|key| stakes.contains_key(key)).count(),
    );
}

/// Run the spy indefinitely, emitting a liveness sample every `interval`.
fn wfsm_spy(
    identity_keypair: Option<Keypair>,
    entrypoint_addrs: &[SocketAddr],
    gossip_addr: &SocketAddr,
    shred_version: u16,
    socket_addr_space: SocketAddrSpace,
    stakes_path: &str,
    interval: Duration,
    include_unstaked: bool,
) -> Result<(), Box<dyn error::Error>> {
    let (mut stakes, mut total_stake) = load_wfsm_stakes(stakes_path)?;
    let mut stakes_mtime = File::open(stakes_path)?.metadata()?.modified()?;

    let exit = Arc::new(AtomicBool::new(false));
    let (_gossip_service, ip_echo, cluster_info) = make_node(
        identity_keypair.unwrap_or_else(Keypair::new),
        entrypoint_addrs,
        exit.clone(),
        Some(gossip_addr),
        shred_version,
        true, // should_check_duplicate_instance
        socket_addr_space,
    );
    let _ip_echo_server = ip_echo.map(|tcp_listener| {
        solana_net_utils::ip_echo_server(
            tcp_listener,
            solana_net_utils::DEFAULT_IP_ECHO_SERVER_THREADS,
            Some(shred_version),
        )
    });
    info!(
        "wfsm spy {} at {gossip_addr}, {} staked identities, sampling every {}ms",
        cluster_info.id(),
        stakes.len(),
        interval.as_millis(),
    );

    while !exit.load(Ordering::Relaxed) {
        // Picked up without a restart so an external job can refresh the
        // snapshot each epoch; a snapshot that fails to parse is left in place
        // rather than taken as an instruction to stop measuring.
        match File::open(stakes_path).and_then(|file| file.metadata()?.modified()) {
            Ok(modified) if modified != stakes_mtime => match load_wfsm_stakes(stakes_path) {
                Ok((fresh, fresh_total)) => {
                    info!("reloaded {stakes_path}: {} staked identities", fresh.len());
                    (stakes, total_stake, stakes_mtime) = (fresh, fresh_total, modified);
                }
                Err(err) => warn!("keeping previous stakes, {stakes_path} unreadable: {err}"),
            },
            Ok(_) => (),
            Err(err) => warn!("cannot stat {stakes_path}: {err}"),
        }
        wfsm_sample(&cluster_info, &stakes, total_stake, include_unstaked);
        thread::sleep(interval);
    }
    Ok(())
}

fn parse_entrypoints(matches: &ArgMatches) -> Vec<SocketAddr> {
    values_t!(matches, "entrypoint", String)
        .unwrap_or_default()
        .into_iter()
        .map(|entrypoint| solana_net_utils::parse_host_port(&entrypoint))
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
}

fn process_rpc_url(
    matches: &ArgMatches,
    socket_addr_space: SocketAddrSpace,
) -> std::io::Result<()> {
    let any = matches.is_present("any");
    let all = matches.is_present("all");
    let timeout = value_t_or_exit!(matches, "timeout", u64);
    let entrypoint_addrs = parse_entrypoints(matches);
    let gossip_addr = get_gossip_address(matches, &entrypoint_addrs);

    let mut shred_version = value_t_or_exit!(matches, "shred_version", u16);
    if shred_version == 0 {
        shred_version = get_entrypoint_shred_version(&entrypoint_addrs)
            .expect("need non-zero shred-version to join the cluster");
    }

    #[allow(deprecated)]
    let (_all_peers, validators) = discover_peers(
        None,
        &entrypoint_addrs,
        Some(1),
        Duration::from_secs(timeout),
        None,
        &entrypoint_addrs,
        Some(&gossip_addr),
        shred_version,
        socket_addr_space,
    )?;

    let rpc_addrs: Vec<_> = validators
        .iter()
        .filter(|node| {
            any || all || {
                #[allow(deprecated)]
                let addrs = node.gossip();
                addrs
                    .map(|addr| entrypoint_addrs.contains(&addr))
                    .unwrap_or_default()
            }
        })
        .filter_map(
            #[allow(deprecated)]
            ContactInfo::rpc,
        )
        .filter(|addr| socket_addr_space.check(addr))
        .collect();

    if rpc_addrs.is_empty() {
        eprintln!("No RPC URL found");
        exit(1);
    }

    for rpc_addr in rpc_addrs {
        println!("http://{rpc_addr}");
        if any {
            break;
        }
    }

    Ok(())
}

fn get_gossip_address(matches: &ArgMatches, entrypoint_addrs: &[SocketAddr]) -> SocketAddr {
    let bind_address = parse_bind_address(matches, entrypoint_addrs);
    SocketAddr::new(
        bind_address,
        value_t!(matches, "gossip_port", u16).unwrap_or_else(|_| {
            solana_net_utils::find_available_port_in_range(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                (0, 1),
            )
            .expect("unable to find an available gossip port")
        }),
    )
}

fn main() -> Result<(), Box<dyn error::Error>> {
    agave_logger::setup_with_default_filter();

    let matches = parse_matches();
    let socket_addr_space = SocketAddrSpace::new(matches.is_present("allow_private_addr"));
    match matches.subcommand() {
        ("spy", Some(matches)) => {
            process_spy(matches, socket_addr_space)?;
        }
        ("rpc-url", Some(matches)) => {
            process_rpc_url(matches, socket_addr_space)?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
