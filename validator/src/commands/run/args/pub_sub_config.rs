use {
    crate::commands::{FromClapArgMatches, Result},
    clap::ArgMatches,
    solana_rpc::rpc_pubsub_service::PubSubConfig,
};

impl FromClapArgMatches for PubSubConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(PubSubConfig {
            enable_block_subscription: matches.is_present("rpc_pubsub_enable_block_subscription"),
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
        solana_rpc::rpc::JsonRpcConfig,
    };

    #[test]
    fn verify_args_struct_by_command_run_with_enable_block_subscription() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                enable_rpc_transaction_history: true,
                ..default_run_args.json_rpc_config.clone()
            },
            pub_sub_config: PubSubConfig {
                enable_block_subscription: true,
                ..default_run_args.pub_sub_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--enable-rpc-transaction-history", // required by enable-rpc-bigtable-ledger-storage
                "--rpc-pubsub-enable-block-subscription",
            ],
            expected_args,
        );
    }
}
