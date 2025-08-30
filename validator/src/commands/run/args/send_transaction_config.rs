use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
    solana_send_transaction_service::send_transaction_service::{
        Config as SendTransactionServiceConfig, MAX_TRANSACTION_SENDS_PER_SECOND,
    },
};

impl FromClapArgMatches for SendTransactionServiceConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let batch_send_rate_ms = value_t!(matches, "rpc_send_transaction_batch_ms", u64)?;
        let retry_rate_ms = value_t!(matches, "rpc_send_transaction_retry_ms", u64)?;
        if batch_send_rate_ms > retry_rate_ms {
            return Err(crate::commands::Error::Dynamic(
                Box::<dyn std::error::Error>::from(format!(
                    "the specified rpc-send-batch-ms ({batch_send_rate_ms}) is invalid, it must \
                 be <= rpc-send-retry-ms ({retry_rate_ms})"
                )),
            ));
        }

        let rpc_send_batch_size = value_t!(matches, "rpc_send_transaction_batch_size", usize)?;
        let millis_per_second = 1000;
        let tps = rpc_send_batch_size as u64 * millis_per_second / batch_send_rate_ms;
        if tps > MAX_TRANSACTION_SENDS_PER_SECOND {
            return Err(crate::commands::Error::Dynamic(Box::<dyn std::error::Error>::from(
                format!(
                    "either the specified rpc-send-batch-size ({rpc_send_batch_size}) or rpc-send-batch-ms ({batch_send_rate_ms}) is invalid, \
                 'rpc-send-batch-size * 1000 / rpc-send-batch-ms' must be smaller than ({MAX_TRANSACTION_SENDS_PER_SECOND}) .",
                ),
            )));
        }

        let tpu_peers = matches
            .values_of("rpc_send_transaction_tpu_peer")
            .map(|values| values.map(solana_net_utils::parse_host_port).collect())
            .transpose()
            .map_err(|e| {
                crate::commands::Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                    "Invalid tpu peer address: {e}",
                )))
            })?;

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
            batch_size: value_t!(matches, "rpc_send_transaction_batch_size", usize)?,
            batch_send_rate_ms,
            default_max_retries: value_t!(
                matches,
                "rpc_send_transaction_default_max_retries",
                usize
            )
            .ok(),
            service_max_retries: value_t!(
                matches,
                "rpc_send_transaction_service_max_retries",
                usize
            )?,
            retry_pool_max_size: value_t!(
                matches,
                "rpc_send_transaction_retry_pool_max_size",
                usize
            )?,
            tpu_peers,
            leader_forward_count,
        })
    }
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
}
