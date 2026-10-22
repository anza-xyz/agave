use {
    crate::cli::{DefaultArgs, thread_args::NumThreadConfig},
    clap::{Arg, ArgMatches, value_t},
    solana_clap_utils::{hidden_unless_forced, input_validators::is_parsable},
    solana_core::validator::ValidatorTpuConfig,
    solana_streamer::{
        nonblocking::swqos::SwQosConfig,
        quic::{QuicStreamerConfig, SwQosQuicStreamerConfig},
    },
    std::error::Error,
};

pub(crate) fn new_config(
    matches: &ArgMatches,
    num_threads: &NumThreadConfig,
) -> Result<ValidatorTpuConfig, Box<dyn Error>> {
    let tpu_max_connections_per_peer =
        value_t!(matches, "tpu_max_connections_per_peer", usize).ok();
    let tpu_max_connections_per_unstaked_peer = tpu_max_connections_per_peer.map_or_else(
        || value_t!(matches, "tpu_max_connections_per_unstaked_peer", usize),
        Ok,
    )?;
    let tpu_max_connections_per_staked_peer = tpu_max_connections_per_peer.map_or_else(
        || value_t!(matches, "tpu_max_connections_per_staked_peer", usize),
        Ok,
    )?;
    let tpu_max_staked_connections = value_t!(matches, "tpu_max_staked_connections", usize)?;
    let tpu_max_unstaked_connections = value_t!(matches, "tpu_max_unstaked_connections", usize)?;

    let tpu_max_fwd_staked_connections =
        value_t!(matches, "tpu_max_fwd_staked_connections", usize)?;
    let tpu_max_fwd_unstaked_connections =
        value_t!(matches, "tpu_max_fwd_unstaked_connections", usize)?;

    let tpu_max_connections_per_ipaddr_per_minute =
        value_t!(matches, "tpu_max_connections_per_ipaddr_per_minute", u64)?;
    let max_streams_per_ms = value_t!(matches, "tpu_max_streams_per_ms", u64)?;

    let tpu_quic_server_config = SwQosQuicStreamerConfig {
        quic_streamer_config: QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
            num_threads: num_threads.tpu_transaction_receive_threads,
            stream_receive_window_size: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
            max_stream_data_bytes: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
            ..Default::default()
        },
        qos_config: SwQosConfig {
            max_connections_per_unstaked_peer: tpu_max_connections_per_unstaked_peer,
            max_connections_per_staked_peer: tpu_max_connections_per_staked_peer,
            max_staked_connections: tpu_max_staked_connections,
            max_unstaked_connections: tpu_max_unstaked_connections,
            max_streams_per_ms,
        },
    };

    let tpu_fwd_quic_server_config = SwQosQuicStreamerConfig {
        quic_streamer_config: QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
            num_threads: num_threads.tpu_transaction_forward_receive_threads,
            stream_receive_window_size: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
            max_stream_data_bytes: solana_message::v1::MAX_TRANSACTION_SIZE as u32,
            ..Default::default()
        },
        qos_config: SwQosConfig {
            max_connections_per_staked_peer: tpu_max_connections_per_staked_peer,
            max_connections_per_unstaked_peer: tpu_max_connections_per_unstaked_peer,
            max_staked_connections: tpu_max_fwd_staked_connections,
            max_unstaked_connections: tpu_max_fwd_unstaked_connections,
            max_streams_per_ms,
        },
    };

    Ok(ValidatorTpuConfig {
        vote_use_quic: false,
        tpu_quic_server_config,
        tpu_fwd_quic_server_config,
        sigverify_threads: num_threads.tpu_sigverify_threads,
    })
}

pub(crate) fn args(default_args: &DefaultArgs) -> Vec<Arg<'_, '_>> {
    vec![
        Arg::with_name("tpu_max_connections_per_ipaddr_per_minute")
            .long("tpu-max-connections-per-ipaddr-per-minute")
            .takes_value(true)
            .default_value(&default_args.tpu_max_connections_per_ipaddr_per_minute)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the rate of the clients connections per IpAddr per minute."),
        Arg::with_name("tpu_max_connections_per_peer")
            .long("tpu-max-connections-per-peer")
            .takes_value(true)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help(
                "Controls the max concurrent connections per IpAddr or staked identity.Overrides \
                 tpu-max-connections-per-unstaked-peer and tpu-max-connections-per-staked-peer",
            ),
        Arg::with_name("tpu_max_connections_per_unstaked_peer")
            .long("tpu-max-connections-per-unstaked-peer")
            .takes_value(true)
            .default_value(&default_args.tpu_max_connections_per_unstaked_peer)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections per IpAddr for unstaked clients."),
        Arg::with_name("tpu_max_connections_per_staked_peer")
            .long("tpu-max-connections-per-staked-peer")
            .takes_value(true)
            .default_value(&default_args.tpu_max_connections_per_staked_peer)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections per staked identity."),
        Arg::with_name("tpu_max_staked_connections")
            .long("tpu-max-staked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_staked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections for TPU from staked nodes."),
        Arg::with_name("tpu_max_unstaked_connections")
            .long("tpu-max-unstaked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_unstaked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections fort TPU from unstaked nodes."),
        Arg::with_name("tpu_max_fwd_staked_connections")
            .long("tpu-max-fwd-staked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_fwd_staked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections for TPU-forward from staked nodes."),
        Arg::with_name("tpu_max_fwd_unstaked_connections")
            .long("tpu-max-fwd-unstaked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_fwd_unstaked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections for TPU-forward from unstaked nodes."),
        Arg::with_name("tpu_max_streams_per_ms")
            .long("tpu-max-streams-per-ms")
            .takes_value(true)
            .default_value(&default_args.tpu_max_streams_per_ms)
            .validator(is_parsable::<usize>)
            .hidden(hidden_unless_forced())
            .help("Controls the max number of streams for a TPU service."),
    ]
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::cli::{app, thread_args::parse_num_threads_args},
    };

    fn new_test_config(args: &[&str]) -> (NumThreadConfig, ValidatorTpuConfig) {
        let default_args = DefaultArgs::default();
        let matches =
            app("test", &default_args).get_matches_from(["agave-validator"].iter().chain(args));
        let num_threads = parse_num_threads_args(&matches);
        let config = new_config(&matches, &num_threads).unwrap();
        (num_threads, config)
    }

    #[test]
    fn test_connection_limits() {
        let (_, config) = new_test_config(&[
            "--tpu-max-connections-per-staked-peer",
            "1",
            "--tpu-max-connections-per-unstaked-peer",
            "2",
            "--tpu-max-staked-connections",
            "3",
            "--tpu-max-unstaked-connections",
            "4",
            "--tpu-max-fwd-staked-connections",
            "5",
            "--tpu-max-fwd-unstaked-connections",
            "6",
        ]);
        let qos = &config.tpu_quic_server_config.qos_config;
        assert_eq!(
            (
                qos.max_connections_per_staked_peer,
                qos.max_connections_per_unstaked_peer,
                qos.max_staked_connections,
                qos.max_unstaked_connections
            ),
            (1, 2, 3, 4)
        );
        let qos = &config.tpu_fwd_quic_server_config.qos_config;
        assert_eq!(
            (
                qos.max_connections_per_staked_peer,
                qos.max_connections_per_unstaked_peer,
                qos.max_staked_connections,
                qos.max_unstaked_connections
            ),
            (1, 2, 5, 6)
        );
    }

    #[test]
    fn test_max_connections_per_peer_overrides_staked_and_unstaked() {
        let (_, config) = new_test_config(&[
            "--tpu-max-connections-per-peer",
            "7",
            "--tpu-max-connections-per-staked-peer",
            "1",
        ]);
        for qos in [
            &config.tpu_quic_server_config.qos_config,
            &config.tpu_fwd_quic_server_config.qos_config,
        ] {
            assert_eq!(
                (
                    qos.max_connections_per_staked_peer,
                    qos.max_connections_per_unstaked_peer
                ),
                (7, 7)
            );
        }
    }

    #[test]
    fn test_thread_counts() {
        let (num_threads, config) = new_test_config(&[
            "--tpu-transaction-receive-threads",
            "2",
            "--tpu-transaction-forward-receive-threads",
            "3",
            "--tpu-sigverify-threads",
            "5",
        ]);
        assert_eq!(
            config
                .tpu_quic_server_config
                .quic_streamer_config
                .num_threads,
            num_threads.tpu_transaction_receive_threads
        );
        assert_eq!(
            config
                .tpu_fwd_quic_server_config
                .quic_streamer_config
                .num_threads,
            num_threads.tpu_transaction_forward_receive_threads
        );
        assert_eq!(config.sigverify_threads, num_threads.tpu_sigverify_threads);
    }
}
