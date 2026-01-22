use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    thread,
    time::Duration,
};

#[cfg(target_os = "linux")]
use {
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        route::Router,
        tx_loop::tx_loop,
    },
    crossbeam_channel::bounded,
};

#[cfg(target_os = "linux")]
use caps::{
    CapSet,
    Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
};

const DEFAULT_COUNT: usize = 5;
const DEFAULT_TIMEOUT_MS: u64 = 1000;
const DEFAULT_PAYLOAD_SIZE: usize = 64;

#[derive(Debug)]
struct Config {
    interface: Option<String>,
    server: SocketAddr,
    cpu: usize,
    count: usize,
    timeout_ms: u64,
    payload_size: usize,
    zero_copy: bool,
}

fn usage() -> ! {
    eprintln!(
        "Usage: xdp-compat-client --server <IP:PORT> [--interface <IFACE>] [--cpu <N>] \
         [--count <N>] [--timeout-ms <MS>] [--payload-size <N>] [--zero-copy]"
    );
    std::process::exit(2);
}

fn parse_args() -> Config {
    let mut args = env::args().skip(1);
    let mut interface = None;
    let mut server = None;
    let mut cpu = 0usize;
    let mut count = DEFAULT_COUNT;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut payload_size = DEFAULT_PAYLOAD_SIZE;
    let mut zero_copy = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interface" => interface = args.next(),
            "--server" => server = args.next(),
            "--cpu" => {
                cpu = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage());
            }
            "--count" => {
                count = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage());
            }
            "--timeout-ms" => {
                timeout_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage());
            }
            "--payload-size" => {
                payload_size = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
            }
            "--zero-copy" => zero_copy = true,
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }

    let server = server
        .and_then(|s| s.parse::<SocketAddr>().ok())
        .unwrap_or_else(|| usage());

    Config {
        interface,
        server,
        cpu,
        count,
        timeout_ms,
        payload_size,
        zero_copy,
    }
}

#[cfg(target_os = "linux")]
fn main() {
    let config = parse_args();
    if !matches!(config.server.ip(), IpAddr::V4(_)) {
        eprintln!("Server must be IPv4 for XDP retransmit.");
        std::process::exit(1);
    }
    let zero_copy = config.zero_copy;

    let dev = match config.interface.as_ref() {
        Some(iface) => NetworkDevice::new(iface.clone()).unwrap_or_else(|e| {
            eprintln!("Failed to open interface {iface}: {e}");
            std::process::exit(1);
        }),
        None => NetworkDevice::new_from_default_route().unwrap_or_else(|e| {
            eprintln!("Failed to resolve default route interface: {e}");
            std::process::exit(1);
        }),
    };
    let iface = dev.name().to_string();

    if let Ok(driver) = dev.driver() {
        if driver == "i40e" {
            eprintln!("Warning: driver i40e has known fragment quirks for XDP.");
        }
    }

    if !is_interface_up(&iface) {
        eprintln!("Interface {iface} is not up.");
        std::process::exit(1);
    }

    let local_ip = dev
        .ipv4_addr()
        .or_else(|_| master_ip_if_bonded(&iface).ok_or_else(|| {
            std::io::Error::other("no IPv4 address on interface or bond master")
        }))
        .unwrap_or_else(|e| {
            eprintln!("Failed to get IPv4 address for {iface}: {e}");
            std::process::exit(1);
        });

    let tx_queues = count_tx_queues(&iface).unwrap_or(0);
    if tx_queues > 0 && config.cpu >= tx_queues {
        eprintln!(
            "CPU core {} exceeds available TX queues ({tx_queues}) on {iface}.",
            config.cpu
        );
        std::process::exit(1);
    }

    let _ebpf = if zero_copy {
        if NetworkDevice::ring_sizes(&iface).is_err() {
            eprintln!("Zero-copy requires NIC ring sizes; none found for {iface}.");
            std::process::exit(1);
        }

        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
            if let Err(e) = caps::raise(None, CapSet::Effective, cap) {
                eprintln!("Failed to raise {cap:?} capability: {e}");
                std::process::exit(1);
            }
        }

        let ebpf = match agave_xdp::load_xdp_program(&dev) {
            Ok(ebpf) => ebpf,
            Err(e) => {
                eprintln!("Failed to attach XDP program in DRV mode: {e}");
                std::process::exit(1);
            }
        };

        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
            let _ = caps::drop(None, CapSet::Effective, cap);
        }
        Some(ebpf)
    } else {
        None
    };

    let server = config.server;
    let udp = UdpSocket::bind(SocketAddr::new(IpAddr::V4(local_ip), 0)).unwrap_or_else(|e| {
        eprintln!("Failed to bind UDP socket: {e}");
        std::process::exit(1);
    });
    udp.set_read_timeout(Some(Duration::from_millis(config.timeout_ms)))
        .unwrap();
    udp.connect(server).unwrap_or_else(|e| {
        eprintln!("Failed to connect UDP socket to {server}: {e}");
        std::process::exit(1);
    });

    // Warm up ARP and route cache using a regular UDP send.
    let _ = udp.send(b"xdp-warmup");
    thread::sleep(Duration::from_millis(100));

    let router = Router::new().unwrap_or_else(|e| {
        eprintln!("Failed to initialize routing table: {e}");
        std::process::exit(1);
    });
    if router.route(server.ip()).is_err() {
        eprintln!("No route to server {server}");
        std::process::exit(1);
    }

    let (sender, receiver) = bounded::<(Vec<SocketAddr>, Vec<u8>)>(1024);
    let cpu_id = config.cpu;
    let src_port = udp.local_addr().unwrap().port();
    let dev = std::sync::Arc::new(dev);
    let router = std::sync::Arc::new(router);

    let tx_thread = {
        let dev = std::sync::Arc::clone(&dev);
        let router = std::sync::Arc::clone(&router);
        thread::spawn(move || {
            let route_router = std::sync::Arc::clone(&router);
            let iface_router = std::sync::Arc::clone(&router);
            tx_loop(
                cpu_id,
                &dev,
                QueueId(cpu_id as u64),
                zero_copy,
                None,
                Some(local_ip),
                src_port,
                receiver,
                bounded(1).0,
                move |ip| {
                    route_router
                        .route(*ip)
                        .ok()
                        .map(|n| (n, route_router.route_version()))
                },
                move |if_index| iface_router.interface_info(if_index).ok().cloned(),
            );
        })
    };

    let mut ok = 0usize;
    for seq in 0..config.count {
        let payload = build_payload(seq, config.payload_size);
        sender.send((vec![server], payload.clone())).unwrap();

        let mut buf = vec![0u8; config.payload_size + 64];
        match udp.recv(&mut buf) {
            Ok(n) => {
                if buf[..n] == payload {
                    ok += 1;
                } else {
                    eprintln!("Response mismatch for seq {seq}");
                }
            }
            Err(e) => {
                eprintln!("Timeout or recv error for seq {seq}: {e}");
            }
        }
    }

    drop(sender);
    let _ = tx_thread.join();

    if ok == config.count {
        println!("XDP compatibility test passed ({ok}/{})", config.count);
        std::process::exit(0);
    }

    eprintln!("XDP compatibility test failed ({ok}/{})", config.count);
    std::process::exit(1);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("XDP compatibility client is Linux-only.");
    std::process::exit(1);
}

fn build_payload(seq: usize, payload_size: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(payload_size.max(16));
    payload.extend_from_slice(b"agave-xdp:");
    payload.extend_from_slice(seq.to_string().as_bytes());
    if payload.len() < payload_size {
        payload.resize(payload_size, b'x');
    }
    payload
}

#[cfg(target_os = "linux")]
fn is_interface_up(iface: &str) -> bool {
    let path = format!("/sys/class/net/{iface}/operstate");
    std::fs::read_to_string(path)
        .map(|s| s.trim() == "up")
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn count_tx_queues(iface: &str) -> Option<usize> {
    let path = format!("/sys/class/net/{iface}/queues");
    let entries = std::fs::read_dir(path).ok()?;
    let count = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("tx-"))
        .count();
    Some(count)
}

#[cfg(target_os = "linux")]
fn master_ip_if_bonded(iface: &str) -> Option<Ipv4Addr> {
    let master_ifindex_path = format!("/sys/class/net/{iface}/master/ifindex");
    let contents = std::fs::read_to_string(master_ifindex_path).ok()?;
    let idx: u32 = contents.trim().parse().ok()?;
    NetworkDevice::new_from_index(idx).ok()?.ipv4_addr().ok()
}
