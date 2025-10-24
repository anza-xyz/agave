use {
    crate::commands::{Error, FromClapArgMatches, Result},
    clap::{value_t, Arg, ArgMatches},
    solana_clap_utils::{
        hidden_unless_forced,
        input_validators::{is_parsable, is_within_range},
    },
    solana_send_transaction_service::send_transaction_service::{
        Config as SendTransactionServiceConfig, MAX_BATCH_SEND_RATE_MS, MAX_TRANSACTION_BATCH_SIZE,
        MAX_TRANSACTION_SENDS_PER_SECOND,
    },
    std::sync::LazyLock,
};

const VALID_RANGE_RPC_SEND_TRANSACTION_BATCH_MS: std::ops::RangeInclusive<usize> =
    1..=MAX_BATCH_SEND_RATE_MS;
const VALID_RANGE_RPC_SEND_TRANSACTION_BATCH_SIZE: std::ops::RangeInclusive<usize> =
    1..=MAX_TRANSACTION_BATCH_SIZE;

static DEFAULT_RPC_SEND_TRANSACTION_BATCH_MS: LazyLock<String> = LazyLock::new(|| {
    SendTransactionServiceConfig::default()
        .batch_send_rate_ms
        .to_string()
});
static DEFAULT_RPC_SEND_TRANSACTION_RETRY_MS: LazyLock<String> = LazyLock::new(|| {
    SendTransactionServiceConfig::default()
        .retry_rate_ms
        .to_string()
});
static DEFAULT_RPC_SEND_TRANSACTION_BATCH_SIZE: LazyLock<String> = LazyLock::new(|| {
    SendTransactionServiceConfig::default()
        .batch_size
        .to_string()
});
static DEFAULT_RPC_SEND_TRANSACTION_SERVICE_MAX_RETRIES: LazyLock<String> = LazyLock::new(|| {
    SendTransactionServiceConfig::default()
        .service_max_retries
        .to_string()
});
static DEFAULT_RPC_SEND_TRANSACTION_RETRY_POOL_MAX_SIZE: LazyLock<String> = LazyLock::new(|| {
    SendTransactionServiceConfig::default()
        .retry_pool_max_size
        .to_string()
});
static DEFAULT_RPC_SEND_TRANSACTION_LEADER_FORWARD_COUNT: LazyLock<String> = LazyLock::new(|| {
    SendTransactionServiceConfig::default()
        .leader_forward_count
        .to_string()
});

impl FromClapArgMatches for SendTransactionServiceConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let batch_send_rate_ms = value_t!(matches, "rpc_send_transaction_batch_ms", u64)?;
        let retry_rate_ms = value_t!(matches, "rpc_send_transaction_retry_ms", u64)?;
        if batch_send_rate_ms > retry_rate_ms {
            return Err(Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                "the specified rpc-send-batch-ms ({batch_send_rate_ms}) is invalid, it must be <= \
                 rpc-send-retry-ms ({retry_rate_ms})"
            ))));
        }

        let batch_size = value_t!(matches, "rpc_send_transaction_batch_size", usize)?;
        let millis_per_second = 1000;
        let tps = batch_size as u64 * millis_per_second / batch_send_rate_ms;
        if tps > MAX_TRANSACTION_SENDS_PER_SECOND {
            return Err(Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                "either the specified rpc-send-batch-size ({batch_size}) or rpc-send-batch-ms \
                 ({batch_send_rate_ms}) is invalid, 'rpc-send-batch-size * 1000 / \
                 rpc-send-batch-ms' must be smaller than ({MAX_TRANSACTION_SENDS_PER_SECOND}) .",
            ))));
        }

        let tpu_peers = matches
            .values_of("rpc_send_transaction_tpu_peer")
            .map(|values| values.map(solana_net_utils::parse_host_port).collect())
            .transpose()
            .map_err(|e| {
                Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                    "Invalid tpu peer address: {e}",
                )))
            })?;

        let default_max_retries =
            value_t!(matches, "rpc_send_transaction_default_max_retries", usize).ok();

        let service_max_retries =
            value_t!(matches, "rpc_send_transaction_service_max_retries", usize)?;

        let retry_pool_max_size =
            value_t!(matches, "rpc_send_transaction_retry_pool_max_size", usize)?;

        let rpc_send_transaction_also_leader =
            matches.is_present("rpc_send_transaction_also_leader");
        let leader_forward_count = if tpu_peers.is_some() && !rpc_send_transaction_also_leader {
            // rpc-sts is configured to send only to specific tpu peers. disable leader forwards
            0
        } else {
            value_t!(matches, "rpc_send_transaction_leader_forward_count", u64)?
        };

        Ok(SendTransactionServiceConfig {
            retry_rate_ms,
            batch_size,
            batch_send_rate_ms,
            default_max_retries,
            service_max_retries,
            retry_pool_max_size,
            tpu_peers,
            leader_forward_count,
        })
    }
}

pub(crate) fn args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::with_name("rpc_send_transaction_batch_ms")
            .long("rpc-send-batch-ms")
            .value_name("MILLISECS")
            .hidden(hidden_unless_forced())
            .takes_value(true)
            .validator(|s| is_within_range(s, VALID_RANGE_RPC_SEND_TRANSACTION_BATCH_MS))
            .default_value(&DEFAULT_RPC_SEND_TRANSACTION_BATCH_MS)
            .help("The rate at which transactions sent via rpc service are sent in batch."),
        Arg::with_name("rpc_send_transaction_retry_ms")
            .long("rpc-send-retry-ms")
            .value_name("MILLISECS")
            .takes_value(true)
            .validator(is_parsable::<u64>)
            .default_value(&DEFAULT_RPC_SEND_TRANSACTION_RETRY_MS)
            .help("The rate at which transactions sent via rpc service are retried."),
        Arg::with_name("rpc_send_transaction_batch_size")
            .long("rpc-send-batch-size")
            .value_name("NUMBER")
            .hidden(hidden_unless_forced())
            .takes_value(true)
            .validator(|s| is_within_range(s, VALID_RANGE_RPC_SEND_TRANSACTION_BATCH_SIZE))
            .default_value(&DEFAULT_RPC_SEND_TRANSACTION_BATCH_SIZE)
            .help("The size of transactions to be sent in batch."),
        Arg::with_name("rpc_send_transaction_tpu_peer")
            .long("rpc-send-transaction-tpu-peer")
            .takes_value(true)
            .number_of_values(1)
            .multiple(true)
            .value_name("HOST:PORT")
            .validator(solana_net_utils::is_host_port)
            .help("Peer(s) to broadcast transactions to instead of the current leader"),
        Arg::with_name("rpc_send_transaction_default_max_retries")
            .long("rpc-send-default-max-retries")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .help(
                "The maximum number of transaction broadcast retries when unspecified by the \
                 request, otherwise retried until expiration.",
            ),
        Arg::with_name("rpc_send_transaction_service_max_retries")
            .long("rpc-send-service-max-retries")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(&DEFAULT_RPC_SEND_TRANSACTION_SERVICE_MAX_RETRIES)
            .help(
                "The maximum number of transaction broadcast retries, regardless of requested \
                 value.",
            ),
        Arg::with_name("rpc_send_transaction_retry_pool_max_size")
            .long("rpc-send-transaction-retry-pool-max-size")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(&DEFAULT_RPC_SEND_TRANSACTION_RETRY_POOL_MAX_SIZE)
            .help("The maximum size of transactions retry pool."),
        Arg::with_name("rpc_send_transaction_also_leader")
            .long("rpc-send-transaction-also-leader")
            .requires("rpc_send_transaction_tpu_peer")
            .help(
                "With `--rpc-send-transaction-tpu-peer HOST:PORT`, also send to the current leader",
            ),
        Arg::with_name("rpc_send_transaction_leader_forward_count")
            .long("rpc-send-leader-count")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<u64>)
            .default_value(&DEFAULT_RPC_SEND_TRANSACTION_LEADER_FORWARD_COUNT)
            .help(
                "The number of upcoming leaders to which to forward transactions sent via rpc \
                 service.",
            ),
    ]
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::run::args::{
            tests::verify_args_struct_by_command_run_with_identity_setup, RunArgs,
        },
        std::net::{Ipv4Addr, SocketAddr},
    };

    #[test]
    fn verify_args_struct_by_command_run_with_retry_rate_ms() {
        let default_run_args = RunArgs::default();
        let expected_args = RunArgs {
            send_transaction_service_config: SendTransactionServiceConfig {
                retry_rate_ms: 99999,
                ..default_run_args.send_transaction_service_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-send-retry-ms", "99999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_batch_size() {
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                send_transaction_service_config: SendTransactionServiceConfig {
                    batch_size: 1,
                    ..default_run_args.send_transaction_service_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-send-batch-size", "1"],
                expected_args,
            );
        }

        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                send_transaction_service_config: SendTransactionServiceConfig {
                    batch_size: 999,
                    batch_send_rate_ms: 1000,
                    ..default_run_args.send_transaction_service_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--rpc-send-batch-size",
                    "999",
                    "--rpc-send-batch-ms",
                    "1000",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_batch_send_rate_ms() {
        let default_run_args = RunArgs::default();
        let expected_args = RunArgs {
            send_transaction_service_config: SendTransactionServiceConfig {
                batch_send_rate_ms: 1999,
                ..default_run_args.send_transaction_service_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-send-batch-ms", "1999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_default_max_retries() {
        let default_run_args = RunArgs::default();
        let expected_args = RunArgs {
            send_transaction_service_config: SendTransactionServiceConfig {
                default_max_retries: Some(9999),
                ..default_run_args.send_transaction_service_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-send-default-max-retries", "9999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_service_max_retries() {
        let default_run_args = RunArgs::default();
        let expected_args = RunArgs {
            send_transaction_service_config: SendTransactionServiceConfig {
                service_max_retries: 9999,
                ..default_run_args.send_transaction_service_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-send-service-max-retries", "9999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_retry_pool_max_size() {
        let default_run_args = RunArgs::default();
        let expected_args = RunArgs {
            send_transaction_service_config: SendTransactionServiceConfig {
                retry_pool_max_size: 9999,
                ..default_run_args.send_transaction_service_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-send-transaction-retry-pool-max-size", "9999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_tpu_peers() {
        // single tpu peer
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                send_transaction_service_config: SendTransactionServiceConfig {
                    tpu_peers: Some(vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 8000))]),
                    leader_forward_count: 0, // see SendTransactionServiceConfig::from_clap_arg_match for more details
                    ..default_run_args.send_transaction_service_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-send-transaction-tpu-peer", "127.0.0.1:8000"],
                expected_args,
            );
        }

        // multiple tpu peers
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                send_transaction_service_config: SendTransactionServiceConfig {
                    tpu_peers: Some(vec![
                        SocketAddr::from((Ipv4Addr::LOCALHOST, 8000)),
                        SocketAddr::from((Ipv4Addr::LOCALHOST, 8001)),
                        SocketAddr::from((Ipv4Addr::LOCALHOST, 8002)),
                    ]),
                    leader_forward_count: 0, // see SendTransactionServiceConfig::from_clap_arg_match for more details
                    ..default_run_args.send_transaction_service_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--rpc-send-transaction-tpu-peer",
                    "127.0.0.1:8000",
                    "--rpc-send-transaction-tpu-peer",
                    "127.0.0.1:8001",
                    "--rpc-send-transaction-tpu-peer",
                    "127.0.0.1:8002",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_send_transaction_leader_forward_count() {
        // rpc-send-transaction-leader-forward-count
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                send_transaction_service_config: SendTransactionServiceConfig {
                    leader_forward_count: 100,
                    ..default_run_args.send_transaction_service_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-send-leader-count", "100"],
                expected_args,
            );
        }

        // rpc-send-transaction-leader-forward-count + rpc-send-transaction-tpu-peer
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                send_transaction_service_config: SendTransactionServiceConfig {
                    leader_forward_count: 0,
                    tpu_peers: Some(vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 8000))]),
                    ..default_run_args.send_transaction_service_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--rpc-send-transaction-tpu-peer",
                    "127.0.0.1:8000",
                    "--rpc-send-leader-count",
                    "100",
                ],
                expected_args,
            );
        }

        // rpc-send-transaction-leader-forward-count + rpc-send-transaction-also-leader + rpc-send-transaction-tpu-peer
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                send_transaction_service_config: SendTransactionServiceConfig {
                    tpu_peers: Some(vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 8000))]),
                    leader_forward_count: 100,
                    ..default_run_args.send_transaction_service_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--rpc-send-transaction-tpu-peer",
                    "127.0.0.1:8000",
                    "--rpc-send-transaction-also-leader",
                    "--rpc-send-leader-count",
                    "100",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn test_default_rpc_send_transaction_batch_ms_unchanged() {
        assert_eq!(*DEFAULT_RPC_SEND_TRANSACTION_BATCH_MS, "1");
    }

    #[test]
    fn test_valid_range_rpc_send_transaction_batch_ms_unchanged() {
        assert_eq!(VALID_RANGE_RPC_SEND_TRANSACTION_BATCH_MS, 1..=100_000);
    }

    #[test]
    fn test_default_rpc_send_transaction_retry_ms_unchanged() {
        assert_eq!(*DEFAULT_RPC_SEND_TRANSACTION_RETRY_MS, "2000");
    }

    #[test]
    fn test_default_rpc_send_transaction_batch_size_unchanged() {
        assert_eq!(*DEFAULT_RPC_SEND_TRANSACTION_BATCH_SIZE, "1");
    }

    #[test]
    fn test_valid_range_rpc_send_transaction_batch_size_unchanged() {
        assert_eq!(VALID_RANGE_RPC_SEND_TRANSACTION_BATCH_SIZE, 1..=10_000);
    }

    #[test]
    fn test_default_rpc_send_transaction_service_max_retries_unchanged() {
        assert_eq!(
            *DEFAULT_RPC_SEND_TRANSACTION_SERVICE_MAX_RETRIES,
            usize::MAX.to_string()
        );
    }

    #[test]
    fn test_default_rpc_send_transaction_retry_pool_max_size_unchanged() {
        assert_eq!(
            *DEFAULT_RPC_SEND_TRANSACTION_RETRY_POOL_MAX_SIZE,
            10_000.to_string()
        );
    }

    #[test]
    fn test_default_rpc_send_transaction_leader_forward_count_unchanged() {
        assert_eq!(*DEFAULT_RPC_SEND_TRANSACTION_LEADER_FORWARD_COUNT, "2");
    }
}
