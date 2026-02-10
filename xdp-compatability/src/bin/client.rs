#[cfg(target_os = "linux")]
mod linux {
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
        clap::{crate_description, crate_name, value_t_or_exit, App, Arg, ArgMatches},
        crossbeam_channel::bounded,
        log::{error, info, warn},
        solana_clap_utils::{
            input_parsers::parse_cpu_ranges, input_validators::validate_cpu_ranges,
        },
        solana_net_utils::sockets::bind_to,
        solana_turbine::xdp::{master_ip_if_bonded, XdpConfig},
        std::{
            net::{IpAddr, SocketAddr, UdpSocket},
            process::exit,
            sync::Arc,
            thread,
            time::{Duration, Instant},
        },
        xdp_compatability::{
            build_request, expected_response, make_token, response_seq, RESPONSE_PREFIX, SEQ_SIZE,
        },
    };

    #[derive(Debug)]
    struct Config {
        xdp_config: XdpConfig,
        server: SocketAddr,
        count: usize,
        timeout_ms: u64,
        max_loss_fraction: f64,
    }

    fn get_clap_app<'ab, 'v>() -> App<'ab, 'v> {
        App::new(crate_name!())
            .about(crate_description!())
            .arg(
                Arg::with_name("target_server")
                    .long("target-server")
                    .value_name("IP:PORT")
                    .takes_value(true)
                    .required(true)
                    .help("Target server address to send XDP compatibility packets to"),
            )
            .arg(
                Arg::with_name("retransmit_xdp_interface")
                    .long("experimental-retransmit-xdp-interface")
                    .value_name("INTERFACE")
                    .takes_value(true)
                    .requires("retransmit_xdp_cpu_cores")
                    .help("EXPERIMENTAL: The network interface to use for XDP retransmit"),
            )
            .arg(
                Arg::with_name("retransmit_xdp_cpu_cores")
                    .long("experimental-retransmit-xdp-cpu-cores")
                    .value_name("CPU_LIST")
                    .takes_value(true)
                    .validator(|value| {
                        validate_cpu_ranges(value, "--experimental-retransmit-xdp-cpu-cores")
                    })
                    .help("EXPERIMENTAL: Enable XDP retransmit on the specified CPU cores"),
            )
            .arg(
                Arg::with_name("retransmit_xdp_zero_copy")
                    .long("experimental-retransmit-xdp-zero-copy")
                    .takes_value(false)
                    .requires("retransmit_xdp_cpu_cores")
                    .help("EXPERIMENTAL: Enable XDP zero copy. Requires hardware support"),
            )
            .arg(
                Arg::with_name("count")
                    .long("count")
                    .value_name("COUNT")
                    .takes_value(true)
                    .default_value("5")
                    .help("Number of test packets to send"),
            )
            .arg(
                Arg::with_name("timeout_ms")
                    .long("timeout-ms")
                    .value_name("MS")
                    .takes_value(true)
                    .default_value("1000")
                    .help("Receive timeout in milliseconds"),
            )
            .arg(
                Arg::with_name("max_loss_fraction")
                    .long("max-loss-fraction")
                    .value_name("FRACTION")
                    .takes_value(true)
                    .default_value("0.0")
                    .help("Allowed loss fraction (0.0-1.0). Timeouts count as loss."),
            )
    }

    fn parse_args() -> Config {
        let matches = get_clap_app().get_matches();
        parse_matches(&matches)
    }

    fn parse_matches(matches: &ArgMatches) -> Config {
        let server = value_t_or_exit!(matches, "target_server", SocketAddr);
        let interface = matches
            .value_of("retransmit_xdp_interface")
            .map(|value| value.to_string());
        let cpu_list = matches.value_of("retransmit_xdp_cpu_cores");
        let count = value_t_or_exit!(matches, "count", usize);
        let timeout_ms = value_t_or_exit!(matches, "timeout_ms", u64);
        let max_loss_fraction = value_t_or_exit!(matches, "max_loss_fraction", f64);
        if !(0.0..=1.0).contains(&max_loss_fraction) {
            error!("--max-loss-fraction must be within [0.0, 1.0]");
            exit(1);
        }
        let zero_copy = matches.is_present("retransmit_xdp_zero_copy");

        let cpus = if let Some(cpu_list) = cpu_list {
            parse_cpu_ranges(cpu_list).unwrap_or_else(|err| {
                error!("--experimental-retransmit-xdp-cpu-cores {err}");
                exit(1);
            })
        } else {
            vec![0]
        };

        let xdp_config = XdpConfig::new(interface, cpus, zero_copy);
        Config {
            xdp_config,
            server,
            count,
            timeout_ms,
            max_loss_fraction,
        }
    }

    enum RecvResult {
        Match,
        Mismatch,
        Timeout,
    }

    fn recv_until_match(udp: &UdpSocket, expected: &[u8], timeout_ms: u64) -> RecvResult {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(timeout_ms))
            .expect("timeout must be less than u64::MAX");
        let mut buf = vec![0u8; expected.len().saturating_add(SEQ_SIZE)];
        let mut saw_packet = false;
        let expected_seq =
            response_seq(expected, expected.len()).expect("expected response must be valid");
        loop {
            if Instant::now() > deadline {
                return if saw_packet {
                    RecvResult::Mismatch
                } else {
                    RecvResult::Timeout
                };
            }
            match udp.recv(&mut buf) {
                Ok(n) => {
                    if n >= RESPONSE_PREFIX.len() && buf[..n].starts_with(RESPONSE_PREFIX) {
                        saw_packet = true;
                        if &buf[..n] == expected {
                            return RecvResult::Match;
                        }
                        if let Some(seq) = response_seq(&buf, n) {
                            if seq != expected_seq {
                                warn!("Received response for different seq {seq}");
                            } else {
                                error!("Received response with mismatched hash for seq {seq}");
                                return RecvResult::Mismatch;
                            }
                        } else {
                            error!("Received malformed agave-xdp response (len {n})");
                            return RecvResult::Mismatch;
                        }
                    } else {
                        warn!("Received stray packet while waiting for response (len {n})");
                    }
                }
                Err(_) => {
                    // Ignore timeout and retry until deadline.
                }
            }
        }
    }

    pub fn main() {
        agave_logger::setup_with_default("info");
        let config = parse_args();
        if !matches!(config.server.ip(), IpAddr::V4(_)) {
            error!("Server must be IPv4 for XDP retransmit.");
            exit(1);
        }
        let xdp_config = &config.xdp_config;
        let zero_copy = xdp_config.zero_copy;

        let dev = match xdp_config.interface.as_ref() {
            Some(iface) => NetworkDevice::new(iface.clone()).unwrap_or_else(|e| {
                error!("Failed to open interface {iface}: {e}");
                exit(1);
            }),
            None => NetworkDevice::new_from_default_route().unwrap_or_else(|e| {
                error!("Failed to resolve default route interface: {e}");
                exit(1);
            }),
        };
        let iface = dev.name().to_string();

        let local_ip = dev
            .ipv4_addr()
            .or_else(|_| {
                master_ip_if_bonded(&iface).ok_or_else(|| {
                    std::io::Error::other("no IPv4 address on interface or bond master")
                })
            })
            .unwrap_or_else(|e| {
                error!("Failed to get IPv4 address for {iface}: {e}");
                exit(1);
            });

        let _ebpf = if zero_copy {
            for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
                if let Err(e) = caps::raise(None, CapSet::Effective, cap) {
                    error!("Failed to raise {cap:?} capability: {e}");
                    exit(1);
                }
            }

            let ebpf = match agave_xdp::load_xdp_program(&dev) {
                Ok(ebpf) => ebpf,
                Err(e) => {
                    error!("Failed to attach XDP program in DRV mode: {e}");
                    exit(1);
                }
            };
            // Keep capabilities raised until AF_XDP socket is created and bound below.
            Some(ebpf)
        } else {
            None
        };

        let server = config.server;
        // Bind a UDP socket to receive responses from the server.
        let udp = bind_to(IpAddr::V4(local_ip), 0).unwrap_or_else(|e| {
            error!("Failed to bind UDP socket: {e}");
            exit(1);
        });
        udp.set_read_timeout(Some(Duration::from_millis(config.timeout_ms)))
            .unwrap();
        udp.connect(server).unwrap_or_else(|e| {
            error!("Failed to connect UDP socket to {server}: {e}");
            exit(1);
        });
        // Warm up ARP/conntrack so the server's reply is accepted by the kernel.
        let _ = udp.send(b"xdp-warmup");
        thread::sleep(Duration::from_millis(100));

        let mut router = Router::new().unwrap_or_else(|e| {
            error!("Failed to initialize routing table: {e}");
            exit(1);
        });
        router.build_caches().unwrap_or_else(|e| {
            error!("Failed to build routing table caches: {e}");
            exit(1);
        });

        let (sender, receiver) = bounded::<(Vec<SocketAddr>, Vec<u8>)>(1024);
        let cpu_id = xdp_config.cpus.first().copied().unwrap_or_else(|| {
            error!("No CPU core configured for XDP retransmit.");
            exit(1);
        });
        if xdp_config.cpus.len() > 1 {
            warn!("Multiple CPU cores supplied; using CPU {cpu_id} for the compatibility client.");
        }
        let src_port = udp.local_addr().unwrap().port();
        let dev = Arc::new(dev);
        let router = Arc::new(router);

        let mut tx_loop_config_builder = TxLoopConfigBuilder::new(src_port);
        tx_loop_config_builder
            .zero_copy(zero_copy)
            .override_src_ip(local_ip);
        let tx_loop_config = tx_loop_config_builder.build_with_src_device(dev.as_ref());

        let tx_loop =
            TxLoopBuilder::new(cpu_id, QueueId(cpu_id as u64), tx_loop_config, dev.as_ref())
                .build();

        if zero_copy {
            for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
                let _ = caps::drop(None, CapSet::Effective, cap);
            }
        }

        let (drop_sender, _drop_receiver) = bounded(1);

        let tx_thread = {
            let router = Arc::clone(&router);
            thread::spawn(move || {
                tx_loop.run(receiver, drop_sender, move |ip| router.route(*ip).ok());
            })
        };

        let mut ok = 0usize;
        let mut mismatches = 0usize;
        let mut timeouts = 0usize;
        for seq in 0..config.count {
            let token = make_token(seq as u64);
            let payload = build_request(seq as u64, token);
            sender.send((vec![server], payload.clone())).unwrap();

            let expected =
                expected_response(&payload).expect("request built by client must be valid");
            match recv_until_match(&udp, &expected, config.timeout_ms) {
                RecvResult::Match => {
                    ok = ok.saturating_add(1);
                }
                RecvResult::Mismatch => {
                    error!("Response mismatch for seq {seq}");
                    mismatches = mismatches.saturating_add(1);
                }
                RecvResult::Timeout => {
                    error!("Response timeout for seq {seq}");
                    timeouts = timeouts.saturating_add(1);
                }
            }
        }

        drop(sender);
        let _ = tx_thread.join();

        let allowed_loss = ((config.count as f64) * config.max_loss_fraction).floor() as usize;
        if mismatches == 0 && timeouts <= allowed_loss {
            info!(
                "XDP compatibility test passed ({ok}/{} w/ {timeouts} timeouts)",
                config.count
            );
            exit(0);
        }

        info!(
            "XDP compatibility test failed ({ok}/{}. {mismatches} mismatches, {timeouts} timeouts)",
            config.count
        );
        exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    pub fn main() {
        eprintln!("XDP compatibility client is Linux-only.");
        std::process::exit(1);
    }
}

fn main() {
    linux::main();
}
