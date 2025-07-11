use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
    solana_rpc::rpc::JsonRpcConfig,
};

impl FromClapArgMatches for JsonRpcConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(JsonRpcConfig {
            enable_rpc_transaction_history: matches.is_present("enable_rpc_transaction_history"),
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
}
