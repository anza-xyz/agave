use std::{
    env,
    net::{IpAddr, SocketAddr, UdpSocket},
    thread,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use {
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        route::Router,
        tx_loop::{TxLoopBuilder, TxLoopConfigBuilder},
    },
    caps::{
        CapSet,
        Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
    },
    crossbeam_channel::bounded,
    solana_net_utils::sockets::bind_to,
    solana_turbine::xdp::master_ip_if_bonded,
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
        "Usage: client --server <IP:PORT> [--interface <IFACE>] [--cpu <N>] [--count <N>] \
         [--timeout-ms <MS>] [--payload-size <N>] [--zero-copy]"
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
                cpu = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
            }
            "--count" => {
                count = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
            }
            "--timeout-ms" => {
                timeout_ms = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
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

    let local_ip = dev
        .ipv4_addr()
        .or_else(|_| {
            master_ip_if_bonded(&iface)
                .ok_or_else(|| std::io::Error::other("no IPv4 address on interface or bond master"))
        })
        .unwrap_or_else(|e| {
            eprintln!("Failed to get IPv4 address for {iface}: {e}");
            std::process::exit(1);
        });

    let _ebpf = if zero_copy {
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
                eprintln!(
                    "Hint: clear any existing XDP with: ip link set {} xdp off",
                    dev.name()
                );
                eprintln!(
                    "Then retry. If it still fails, run without --zero-copy (copy mode does not \
                     require XDP)."
                );
                std::process::exit(1);
            }
        };
        // Keep capabilities raised until AF_XDP socket is created and bound below.
        Some(ebpf)
    } else {
        None
    };

    let server = config.server;
    // Bind a kernel UDP socket to trigger ARP/neighbor resolution before XDP sends.
    let udp = bind_to(IpAddr::V4(local_ip), 0).unwrap_or_else(|e| {
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

    let mut tx_loop_config_builder = TxLoopConfigBuilder::new(src_port);
    tx_loop_config_builder
        .zero_copy(zero_copy)
        .override_src_ip(local_ip);
    let tx_loop_config = tx_loop_config_builder.build_with_src_device(dev.as_ref());

    let tx_loop =
        TxLoopBuilder::new(cpu_id, QueueId(cpu_id as u64), tx_loop_config, dev.as_ref()).build();

    if zero_copy {
        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
            let _ = caps::drop(None, CapSet::Effective, cap);
        }
    }

    let (drop_sender, _drop_receiver) = bounded(1);

    let tx_thread = {
        let router = std::sync::Arc::clone(&router);
        thread::spawn(move || {
            tx_loop.run(receiver, drop_sender, move |ip| router.route(*ip).ok());
        })
    };

    let mut ok = 0usize;
    for seq in 0..config.count {
        let payload = xdp_compatability::build_request(seq as u64, config.payload_size);
        sender.send((vec![server], payload.clone())).unwrap();

        let expected = xdp_compatability::expected_response(&payload);
        if recv_until_match(&udp, &expected, config.timeout_ms) {
            ok = ok.saturating_add(1);
        } else {
            eprintln!("Response mismatch or timeout for seq {seq}");
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

fn recv_until_match(udp: &UdpSocket, payload: &[u8], timeout_ms: u64) -> bool {
    let deadline = Instant::now().checked_add(Duration::from_millis(timeout_ms));
    let mut buf = vec![0u8; payload.len().saturating_add(64)];
    loop {
        let Some(deadline) = deadline else {
            return false;
        };
        if Instant::now() > deadline {
            return false;
        }
        match udp.recv(&mut buf) {
            Ok(n) => {
                if &buf[..n] == payload {
                    return true;
                }
            }
            Err(_) => {
                // Ignore timeout and retry until deadline.
            }
        }
    }
}
