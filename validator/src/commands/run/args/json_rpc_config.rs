use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, Arg, ArgMatches},
    solana_accounts_db::accounts_index::AccountSecondaryIndexes,
    solana_clap_utils::input_validators::is_parsable,
    solana_rpc::rpc::{JsonRpcConfig, RpcBigtableConfig},
    std::sync::LazyLock,
};

static DEFAULT_HEALTH_CHECK_SLOT_DISTANCE: LazyLock<String> = LazyLock::new(|| {
    solana_rpc_client_api::request::DELINQUENT_VALIDATOR_SLOT_DISTANCE.to_string()
});
static DEFAULT_MAX_MULTIPLE_ACCOUNTS: LazyLock<String> =
    LazyLock::new(|| solana_rpc_client_api::request::MAX_MULTIPLE_ACCOUNTS.to_string());
static DEFAULT_RPC_THREADS: LazyLock<String> = LazyLock::new(|| num_cpus::get().to_string());
static DEFAULT_RPC_BLOCKING_THREADS: LazyLock<String> =
    LazyLock::new(|| (1.max(num_cpus::get() / 4)).to_string());
const DEFAULT_RPC_NICENESS_ADJ: &str = "0";
static DEFAULT_RPC_MAX_REQUEST_BODY_SIZE: LazyLock<String> =
    LazyLock::new(|| solana_rpc::rpc::MAX_REQUEST_BODY_SIZE.to_string());

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

pub(crate) fn args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::with_name("enable_rpc_transaction_history")
            .long("enable-rpc-transaction-history")
            .takes_value(false)
            .help(
                "Enable historical transaction info over JSON RPC, including the \
                 'getConfirmedBlock' API. This will cause an increase in disk usage and IOPS",
            ),
        Arg::with_name("enable_rpc_bigtable_ledger_storage")
            .long("enable-rpc-bigtable-ledger-storage")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help(
                "Fetch historical transaction info from a BigTable instance as a fallback to \
                 local ledger data",
            ),
        Arg::with_name("enable_extended_tx_metadata_storage")
            .long("enable-extended-tx-metadata-storage")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help(
                "Include CPI inner instructions, logs, and return data in the historical \
                 transaction info stored",
            ),
        Arg::with_name("rpc_faucet_addr")
            .long("rpc-faucet-address")
            .value_name("HOST:PORT")
            .takes_value(true)
            .validator(solana_net_utils::is_host_port)
            .help("Enable the JSON RPC 'requestAirdrop' API with this faucet address."),
        Arg::with_name("health_check_slot_distance")
            .long("health-check-slot-distance")
            .value_name("SLOT_DISTANCE")
            .takes_value(true)
            .default_value(&DEFAULT_HEALTH_CHECK_SLOT_DISTANCE)
            .help(
                "Report this validator as healthy if its latest replayed optimistically confirmed \
                 slot is within the specified number of slots from the cluster's latest \
                 optimistically confirmed slot",
            ),
        Arg::with_name("skip_preflight_health_check")
            .long("skip-preflight-health-check")
            .takes_value(false)
            .help("Skip health check when running a preflight check"),
        Arg::with_name("rpc_max_multiple_accounts")
            .long("rpc-max-multiple-accounts")
            .value_name("MAX ACCOUNTS")
            .takes_value(true)
            .default_value(&DEFAULT_MAX_MULTIPLE_ACCOUNTS)
            .help(
                "Override the default maximum accounts accepted by the getMultipleAccounts JSON \
                 RPC method",
            ),
        Arg::with_name("rpc_threads")
            .long("rpc-threads")
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .default_value(&DEFAULT_RPC_THREADS)
            .help("Number of threads to use for servicing RPC requests"),
        Arg::with_name("rpc_blocking_threads")
            .long("rpc-blocking-threads")
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .validator(|value| {
                value
                    .parse::<u64>()
                    .map_err(|err| format!("error parsing '{value}': {err}"))
                    .and_then(|threads| {
                        if threads > 0 {
                            Ok(())
                        } else {
                            Err("value must be >= 1".to_string())
                        }
                    })
            })
            .takes_value(true)
            .default_value(&DEFAULT_RPC_BLOCKING_THREADS)
            .help(
                "Number of blocking threads to use for servicing CPU bound RPC requests (eg \
                 getMultipleAccounts)",
            ),
        Arg::with_name("rpc_niceness_adj")
            .long("rpc-niceness-adjustment")
            .value_name("ADJUSTMENT")
            .takes_value(true)
            .validator(solana_perf::thread::is_niceness_adjustment_valid)
            .default_value(DEFAULT_RPC_NICENESS_ADJ)
            .help(
                "Add this value to niceness of RPC threads. Negative value increases priority, \
                 positive value decreases priority.",
            ),
        Arg::with_name("full_rpc_api")
            .long("full-rpc-api")
            .takes_value(false)
            .help("Expose RPC methods for querying chain state and transaction history"),
        Arg::with_name("rpc_scan_and_fix_roots")
            .long("rpc-scan-and-fix-roots")
            .takes_value(false)
            .requires("enable_rpc_transaction_history")
            .help("Verifies blockstore roots on boot and fixes any gaps"),
        Arg::with_name("rpc_max_request_body_size")
            .long("rpc-max-request-body-size")
            .value_name("BYTES")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(&DEFAULT_RPC_MAX_REQUEST_BODY_SIZE)
            .help("The maximum request body size accepted by rpc service"),
    ]
}

#[cfg(test)]
pub(super) mod tests {
    #[cfg(not(target_os = "linux"))]
    use crate::commands::run::args::tests::verify_args_struct_by_command_run_is_error_with_identity_setup;
    use {
        super::*,
        crate::commands::run::args::{
            pub_sub_config::DEFAULT_RPC_PUBSUB_NUM_NOTIFICATION_THREADS,
            tests::verify_args_struct_by_command_run_with_identity_setup, RunArgs,
        },
        solana_rpc::rpc_pubsub_service::PubSubConfig,
        std::{
            net::{Ipv4Addr, SocketAddr},
            num::NonZeroUsize,
        },
    };

    pub fn default_json_rpc_config() -> JsonRpcConfig {
        JsonRpcConfig {
            health_check_slot_distance: DEFAULT_HEALTH_CHECK_SLOT_DISTANCE.parse().unwrap(),
            max_multiple_accounts: Some(DEFAULT_MAX_MULTIPLE_ACCOUNTS.parse().unwrap()),
            rpc_threads: DEFAULT_RPC_THREADS.parse().unwrap(),
            rpc_blocking_threads: DEFAULT_RPC_BLOCKING_THREADS.parse().unwrap(),
            rpc_niceness_adj: DEFAULT_RPC_NICENESS_ADJ.parse().unwrap(),
            max_request_body_size: Some(DEFAULT_RPC_MAX_REQUEST_BODY_SIZE.parse().unwrap()),
            ..JsonRpcConfig::default()
        }
    }

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
                    "--enable-rpc-transaction-history", // required by enable_extended_tx_metadata_storage
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

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn verify_args_struct_by_command_run_with_rpc_niceness_adj() {
        verify_args_struct_by_command_run_is_error_with_identity_setup(
            crate::commands::run::args::RunArgs::default(),
            vec!["--rpc-niceness-adjustment", "10"],
        );
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
                pub_sub_config: PubSubConfig {
                    notification_threads: Some(
                        NonZeroUsize::new(
                            DEFAULT_RPC_PUBSUB_NUM_NOTIFICATION_THREADS
                                .parse::<usize>()
                                .unwrap(),
                        )
                        .unwrap(),
                    ),
                    ..default_run_args.pub_sub_config.clone()
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

    #[test]
    fn test_default_health_check_slot_distance_unchanged() {
        assert_eq!(*DEFAULT_HEALTH_CHECK_SLOT_DISTANCE, "128");
    }

    #[test]
    fn test_default_max_multiple_accounts_unchanged() {
        assert_eq!(*DEFAULT_MAX_MULTIPLE_ACCOUNTS, "100");
    }

    #[test]
    fn test_default_rpc_threads_unchanged() {
        assert_eq!(*DEFAULT_RPC_THREADS, num_cpus::get().to_string());
    }

    #[test]
    fn test_default_rpc_blocking_threads_unchanged() {
        assert_eq!(
            *DEFAULT_RPC_BLOCKING_THREADS,
            (1.max(num_cpus::get() / 4)).to_string(),
        );
    }

    #[test]
    fn test_default_rpc_niceness_adj_unchanged() {
        assert_eq!(DEFAULT_RPC_NICENESS_ADJ, "0");
    }

    #[test]
    fn test_default_rpc_max_request_body_size_unchanged() {
        assert_eq!(*DEFAULT_RPC_MAX_REQUEST_BODY_SIZE, 51_200.to_string());
    }
}
