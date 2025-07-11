use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
    solana_rpc::rpc::RpcBigtableConfig,
    std::time::Duration,
};

impl FromClapArgMatches for RpcBigtableConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(RpcBigtableConfig {
            enable_bigtable_ledger_upload: matches.is_present("enable_bigtable_ledger_upload"),
            bigtable_instance_name: value_t!(matches, "rpc_bigtable_instance_name", String)?,
            bigtable_app_profile_id: value_t!(matches, "rpc_bigtable_app_profile_id", String)?,
            timeout: value_t!(matches, "rpc_bigtable_timeout", u64)
                .ok()
                .map(Duration::from_secs),
            max_message_size: value_t!(matches, "rpc_bigtable_max_message_size", usize)?,
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
        solana_rpc::rpc::JsonRpcConfig,
    };

    fn default_rpc_bigtable_config() -> RpcBigtableConfig {
        RpcBigtableConfig {
            timeout: Some(Duration::from_secs(30)),
            ..RpcBigtableConfig::default()
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_enable_rpc_bigtable_ledger_storage() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                enable_rpc_transaction_history: true,
                rpc_bigtable_config: Some(RpcBigtableConfig {
                    ..default_rpc_bigtable_config()
                }),
                ..default_run_args.json_rpc_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--enable-rpc-transaction-history", // required by enable-rpc-bigtable-ledger-storage
                "--enable-rpc-bigtable-ledger-storage",
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_enable_bigtable_ledger_upload() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                enable_rpc_transaction_history: true,
                rpc_bigtable_config: Some(RpcBigtableConfig {
                    enable_bigtable_ledger_upload: true,
                    ..default_rpc_bigtable_config()
                }),
                ..default_run_args.json_rpc_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--enable-rpc-transaction-history", // required by enable-bigtable-ledger-upload
                "--enable-bigtable-ledger-upload",
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_bigtable_instance_name() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                enable_rpc_transaction_history: true,
                rpc_bigtable_config: Some(RpcBigtableConfig {
                    enable_bigtable_ledger_upload: true,
                    bigtable_instance_name: "my-custom-instance-name".to_string(),
                    ..default_rpc_bigtable_config()
                }),
                ..default_run_args.json_rpc_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--enable-rpc-transaction-history", // required by enable-bigtable-ledger-upload
                "--enable-bigtable-ledger-upload",  // required by all rpc_bigtable_config
                "--rpc-bigtable-instance-name",
                "my-custom-instance-name",
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_bigtable_app_profile_id() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                enable_rpc_transaction_history: true,
                rpc_bigtable_config: Some(RpcBigtableConfig {
                    enable_bigtable_ledger_upload: true,
                    bigtable_app_profile_id: "my-custom-app-profile-id".to_string(),
                    ..default_rpc_bigtable_config()
                }),
                ..default_run_args.json_rpc_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--enable-rpc-transaction-history", // required by enable-bigtable-ledger-upload
                "--enable-bigtable-ledger-upload",  // required by all rpc_bigtable_config
                "--rpc-bigtable-app-profile-id",
                "my-custom-app-profile-id",
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_bigtable_timeout() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                enable_rpc_transaction_history: true,
                rpc_bigtable_config: Some(RpcBigtableConfig {
                    enable_bigtable_ledger_upload: true,
                    timeout: Some(Duration::from_secs(99999)),
                    ..default_rpc_bigtable_config()
                }),
                ..default_run_args.json_rpc_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--enable-rpc-transaction-history", // required by enable-bigtable-ledger-upload
                "--enable-bigtable-ledger-upload",  // required by all rpc_bigtable_config
                "--rpc-bigtable-timeout",
                "99999",
            ],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_rpc_bigtable_max_message_size() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                enable_rpc_transaction_history: true,
                rpc_bigtable_config: Some(RpcBigtableConfig {
                    enable_bigtable_ledger_upload: true,
                    max_message_size: 99999,
                    ..default_rpc_bigtable_config()
                }),
                ..default_run_args.json_rpc_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--enable-rpc-transaction-history", // required by enable-bigtable-ledger-upload
                "--enable-bigtable-ledger-upload",  // required by all rpc_bigtable_config
                "--rpc-bigtable-max-message-size",
                "99999",
            ],
            expected_args,
        );
    }
}
