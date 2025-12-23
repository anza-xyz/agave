use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{Arg, ArgMatches, value_t},
    solana_clap_utils::{hidden_unless_forced, input_validators::is_parsable},
    solana_rpc::rpc::RpcBigtableConfig,
    std::{sync::LazyLock, time::Duration},
};

const DEFAULT_BIGTABLE_INSTANCE_NAME: &str = solana_storage_bigtable::DEFAULT_INSTANCE_NAME;
const DEFAULT_BIGTABLE_APP_PROFILE_ID: &str = solana_storage_bigtable::DEFAULT_APP_PROFILE_ID;
const DEFAULT_BIGTABLE_TIMEOUT: &str = "30";
static DEFAULT_BIGTABLE_MAX_MESSAGE_SIZE: LazyLock<String> =
    LazyLock::new(|| solana_storage_bigtable::DEFAULT_MAX_MESSAGE_SIZE.to_string());

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

pub(crate) fn args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::with_name("enable_bigtable_ledger_upload")
            .long("enable-bigtable-ledger-upload")
            .takes_value(false)
            .hidden(hidden_unless_forced())
            .help("Upload new confirmed blocks into a BigTable instance"),
        Arg::with_name("rpc_bigtable_instance_name")
            .long("rpc-bigtable-instance-name")
            .takes_value(true)
            .value_name("INSTANCE_NAME")
            .default_value(DEFAULT_BIGTABLE_INSTANCE_NAME)
            .help("Name of the Bigtable instance to upload to"),
        Arg::with_name("rpc_bigtable_app_profile_id")
            .long("rpc-bigtable-app-profile-id")
            .takes_value(true)
            .value_name("APP_PROFILE_ID")
            .default_value(DEFAULT_BIGTABLE_APP_PROFILE_ID)
            .help("Bigtable application profile id to use in requests"),
        Arg::with_name("rpc_bigtable_timeout")
            .long("rpc-bigtable-timeout")
            .value_name("SECONDS")
            .validator(is_parsable::<u64>)
            .takes_value(true)
            .default_value(DEFAULT_BIGTABLE_TIMEOUT)
            .help("Number of seconds before timing out RPC requests backed by BigTable"),
        Arg::with_name("rpc_bigtable_max_message_size")
            .long("rpc-bigtable-max-message-size")
            .value_name("BYTES")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .default_value(&DEFAULT_BIGTABLE_MAX_MESSAGE_SIZE)
            .help("Max encoding and decoding message size used in Bigtable Grpc client"),
    ]
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::run::args::{
            RunArgs, tests::verify_args_struct_by_command_run_with_identity_setup,
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

    #[test]
    fn test_default_bigtable_instance_name_unchanged() {
        assert_eq!(DEFAULT_BIGTABLE_INSTANCE_NAME, "solana-ledger");
    }

    #[test]
    fn test_default_bigtable_app_profile_id_unchanged() {
        assert_eq!(DEFAULT_BIGTABLE_APP_PROFILE_ID, "default");
    }

    #[test]
    fn test_default_bigtable_timeout_unchanged() {
        assert_eq!(DEFAULT_BIGTABLE_TIMEOUT, "30");
    }

    #[test]
    fn test_default_bigtable_max_message_size_unchanged() {
        assert_eq!(*DEFAULT_BIGTABLE_MAX_MESSAGE_SIZE, "67108864");
    }
}
