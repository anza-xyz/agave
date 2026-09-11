//! A command-line executable for monitoring a cluster's gossip plane.
#[allow(deprecated)]
use solana_gossip::{
    contact_info::ContactInfo,
    gossip_service::{discover_peers, discover_peers_and_inspect},
};
use {
    clap::{
        App, AppSettings, Arg, ArgMatches, SubCommand, crate_description, crate_name, value_t,
        value_t_or_exit, values_t,
    },
    log::{info, warn},
    serde_json::{Map, Value, json},
    solana_clap_utils::{
        hidden_unless_forced,
        input_parsers::{keypair_of, pubkeys_of},
        input_validators::{is_keypair_or_ask_keyword, is_port, is_pubkey},
    },
    solana_gossip::{
        contact_info::socket_tag_name,
        ping_probe::{PingProbeConfig, TargetStats, run_ping_probe},
    },
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        error,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        process::exit,
        time::Duration,
    },
};

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
                    Arg::with_name("dump")
                        .long("dump")
                        .value_name("PATH")
                        .takes_value(true)
                        .help(
                            "Dump every discovered ContactInfo, plus the slots each node voted \
                             on, to PATH as JSON lines (- for stdout)",
                        ),
                ),
        )
        .subcommand(
            SubCommand::with_name("ping")
                .about("Measure gossip ping/pong loss and round-trip time against endpoints")
                .setting(AppSettings::DisableVersion)
                .arg(
                    Arg::with_name("target")
                        .short("t")
                        .long("target")
                        .value_name("HOST:PORT")
                        .takes_value(true)
                        .multiple(true)
                        .required(true)
                        .validator(solana_net_utils::is_host_port)
                        .help("Gossip endpoint to ping. May be given more than once"),
                )
                .arg(
                    Arg::with_name("identity")
                        .short("i")
                        .long("identity")
                        .value_name("PATH")
                        .takes_value(true)
                        .validator(is_keypair_or_ask_keyword)
                        .help("Identity keypair to sign pings with [default: ephemeral keypair]"),
                )
                .arg(
                    Arg::with_name("count")
                        .short("c")
                        .long("count")
                        .value_name("NUM")
                        .takes_value(true)
                        .default_value("10")
                        .help("Number of pings to send to each target, or 0 to ping forever"),
                )
                .arg(
                    Arg::with_name("interval")
                        .long("interval")
                        .value_name("MILLIS")
                        .takes_value(true)
                        .default_value("500")
                        .help("Delay between successive pings to the same target"),
                )
                .arg(
                    Arg::with_name("timeout")
                        .long("timeout")
                        .value_name("MILLIS")
                        .takes_value(true)
                        .default_value("500")
                        .help("A pong arriving later than this counts as lost"),
                )
                .arg(
                    Arg::with_name("report_interval")
                        .long("report-interval")
                        .value_name("SECONDS")
                        .takes_value(true)
                        .default_value("10")
                        .help("How often to log an interim summary while running"),
                )
                .arg(
                    Arg::with_name("json")
                        .long("json")
                        .takes_value(false)
                        .help("Print the summary as JSON lines, one object per target"),
                )
                .arg(&bind_address_arg),
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

/// Dumps one JSON object per line: every discovered ContactInfo together with
/// the slots of the votes that node gossiped, followed by vote-only records for
/// nodes which have votes in CRDS but no (longer a) contact info.
fn dump_gossip_state(
    path: &str,
    nodes: &[ContactInfo],
    mut vote_slots: HashMap<Pubkey, Vec<u64>>,
) -> std::io::Result<()> {
    let mut out = String::new();
    for node in nodes {
        let sockets: Map<String, Value> = node
            .iter_sockets()
            .map(|(tag, addr)| {
                let name = socket_tag_name(tag)
                    .map_or_else(|| format!("unknown_{tag}"), ToString::to_string);
                (name, Value::from(addr.to_string()))
            })
            .collect();
        let entry = json!({
            "pubkey": node.pubkey().to_string(),
            "wallclock": node.wallclock(),
            "outset": node.outset(),
            "shred_version": node.shred_version(),
            "version": node.version().to_string(),
            "sockets": sockets,
            "vote_slots": vote_slots.remove(node.pubkey()).unwrap_or_default(),
        });
        out.push_str(&entry.to_string());
        out.push('\n');
    }
    for (pubkey, slots) in vote_slots {
        let entry = json!({
            "pubkey": pubkey.to_string(),
            "vote_slots": slots,
        });
        out.push_str(&entry.to_string());
        out.push('\n');
    }
    if path == "-" {
        print!("{out}");
        Ok(())
    } else {
        std::fs::write(path, out)
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

    let discover_timeout = Duration::from_secs(timeout.unwrap_or(u64::MAX));
    let dump_path = matches.value_of("dump");
    #[allow(deprecated)]
    let (all_peers, validators, vote_slots) = discover_peers_and_inspect(
        identity_keypair,
        &entrypoint_addrs,
        num_nodes,
        discover_timeout,
        pubkeys.as_deref(),
        &[],
        Some(&gossip_addr),
        shred_version,
        socket_addr_space,
        |cluster_info| {
            dump_path
                .map(|_| cluster_info.get_all_vote_slots())
                .unwrap_or_default()
        },
    )?;

    if let Some(path) = dump_path {
        dump_gossip_state(path, &all_peers, vote_slots)?;
    }

    process_spy_results(
        timeout,
        validators,
        num_nodes,
        num_nodes_exactly,
        pubkeys.as_deref(),
    );

    Ok(())
}

fn process_ping(matches: &ArgMatches) -> std::io::Result<()> {
    let targets: Vec<SocketAddr> = values_t!(matches, "target", String)
        .unwrap_or_default()
        .iter()
        .map(|target| {
            solana_net_utils::parse_host_port(target).unwrap_or_else(|e| {
                eprintln!("failed to parse target {target}: {e}");
                exit(1);
            })
        })
        .collect();
    let count = value_t_or_exit!(matches, "count", u64);
    let config = PingProbeConfig {
        bind_ip: matches.value_of("bind_address").map_or(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            |bind_address| {
                solana_net_utils::parse_host(bind_address).unwrap_or_else(|e| {
                    eprintln!("failed to parse bind-address: {e}");
                    exit(1);
                })
            },
        ),
        interval: Duration::from_millis(value_t_or_exit!(matches, "interval", u64)),
        timeout: Duration::from_millis(value_t_or_exit!(matches, "timeout", u64)),
        count: (count != 0).then_some(count),
        report_interval: Duration::from_secs(value_t_or_exit!(matches, "report_interval", u64)),
    };
    let stats = run_ping_probe(&targets, keypair_of(matches, "identity"), &config)?;
    if matches.is_present("json") {
        for stats in &stats {
            println!("{}", ping_stats_json(stats));
        }
    } else {
        for stats in &stats {
            print_ping_stats(stats);
        }
    }
    Ok(())
}

fn ping_stats_json(stats: &TargetStats) -> Value {
    let rtt_ms = stats.rtt_summary().map(|rtt| {
        json!({
            "min": millis(rtt.min),
            "p50": millis(rtt.p50),
            "mean": millis(rtt.mean),
            "p99": millis(rtt.p99),
            "max": millis(rtt.max),
            "stddev": millis(rtt.stddev),
        })
    });
    let responders: Map<String, Value> = stats
        .responders
        .iter()
        .map(|(pubkey, count)| (pubkey.to_string(), Value::from(*count)))
        .collect();
    json!({
        "target": stats.addr.to_string(),
        "sent": stats.sent,
        "received": stats.received,
        "loss_percent": stats.loss_percent(),
        "late": stats.late,
        "invalid": stats.invalid,
        "send_errors": stats.send_errors,
        "rtt_ms": rtt_ms,
        "responders": responders,
    })
}

fn print_ping_stats(stats: &TargetStats) {
    println!("{}", stats.addr);
    println!(
        "  sent {}  received {}  loss {:.2}%  late {}  invalid {}  send-errors {}",
        stats.sent,
        stats.received,
        stats.loss_percent(),
        stats.late,
        stats.invalid,
        stats.send_errors,
    );
    match stats.rtt_summary() {
        Some(rtt) => println!(
            "  rtt ms  min {:.3}  p50 {:.3}  mean {:.3}  p99 {:.3}  max {:.3}  stddev {:.3}",
            millis(rtt.min),
            millis(rtt.p50),
            millis(rtt.mean),
            millis(rtt.p99),
            millis(rtt.max),
            millis(rtt.stddev),
        ),
        None => println!("  rtt ms  no replies in time"),
    }
    let mut responders: Vec<_> = stats.responders.iter().collect();
    responders.sort_unstable_by_key(|(_pubkey, count)| std::cmp::Reverse(**count));
    for (pubkey, count) in responders {
        println!("  responder {pubkey}  {count} pong(s)");
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
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
        ("ping", Some(matches)) => {
            process_ping(matches)?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
