use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
    solana_rpc::rpc::JsonRpcConfig,
};

impl FromClapArgMatches for JsonRpcConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
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
            ..Default::default()
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
}
