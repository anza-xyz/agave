use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
    solana_accounts_db::accounts_index::AccountSecondaryIndexes,
    solana_rpc::rpc::{JsonRpcConfig, RpcBigtableConfig},
};

impl FromClapArgMatches for JsonRpcConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let rpc_bigtable_config = if matches.is_present("enable_rpc_bigtable_ledger_storage")
            || matches.is_present("enable_bigtable_ledger_upload")
        {
            Some(RpcBigtableConfig::from_clap_arg_match(matches)?)
        } else {
            None
        };

        Ok(JsonRpcConfig {
            enable_rpc_transaction_history: matches.is_present("enable_rpc_transaction_history"),
            enable_extended_tx_metadata_storage: matches
                .is_present("enable_extended_tx_metadata_storage"),
            faucet_addr: matches
                .value_of("rpc_faucet_addr")
                .map(|address| {
                    solana_net_utils::parse_host_port(address).map_err(|err| {
                        crate::commands::Error::Dynamic(Box::<dyn std::error::Error>::from(
                            format!("failed to parse rpc_faucet_addr: {err}"),
                        ))
                    })
                })
                .transpose()?,
            health_check_slot_distance: value_t!(matches, "health_check_slot_distance", u64)?,
            skip_preflight_health_check: matches.is_present("skip_preflight_health_check"),
            rpc_bigtable_config,
            max_multiple_accounts: Some(value_t!(matches, "rpc_max_multiple_accounts", usize)?),
            account_indexes: AccountSecondaryIndexes::from_clap_arg_match(matches)?,
            rpc_threads: value_t!(matches, "rpc_threads", usize)?,
            rpc_blocking_threads: value_t!(matches, "rpc_blocking_threads", usize)?,
            rpc_niceness_adj: value_t!(matches, "rpc_niceness_adj", i8)?,
            full_api: matches.is_present("full_rpc_api"),
            rpc_scan_and_fix_roots: matches.is_present("rpc_scan_and_fix_roots"),
            max_request_body_size: Some(value_t!(matches, "rpc_max_request_body_size", usize)?),
            disable_health_check: false,
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
    fn verify_args_struct_by_command_run_with_enable_rpc_transaction_history() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    enable_rpc_transaction_history: true,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--enable-rpc-transaction-history"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_enable_extended_tx_metadata_storage() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    enable_rpc_transaction_history: true,
                    enable_extended_tx_metadata_storage: true,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--enable-rpc-transaction-history", // required for enable_extended_tx_metadata_storage
                    "--enable-extended-tx-metadata-storage",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_faucet_addr() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    faucet_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 8000))),
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-faucet-address", "127.0.0.1:8000"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_health_check_slot_distance() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    health_check_slot_distance: 100,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--health-check-slot-distance", "100"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_skip_preflight_health_check() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    skip_preflight_health_check: true,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--skip-preflight-health-check"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_max_multiple_accounts() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    max_multiple_accounts: Some(9999),
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-max-multiple-accounts", "9999"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_threads() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    rpc_threads: 10,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-threads", "10"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_blocking_threads() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    rpc_blocking_threads: 999,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-blocking-threads", "999"],
                expected_args,
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_args_struct_by_command_run_with_rpc_niceness_adj() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    rpc_niceness_adj: 10,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-niceness-adjustment", "10"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_full_api() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    full_api: true,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--full-rpc-api"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_scan_and_fix_roots() {
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    enable_rpc_transaction_history: true,
                    rpc_scan_and_fix_roots: true,
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--enable-rpc-transaction-history", // required by --rpc-scan-and-fix-roots
                    "--rpc-scan-and-fix-roots",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_max_request_body_size() {
        // long arg
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                json_rpc_config: JsonRpcConfig {
                    max_request_body_size: Some(999),
                    ..default_run_args.json_rpc_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--rpc-max-request-body-size", "999"],
                expected_args,
            );
        }
    }
}
