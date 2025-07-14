use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
    solana_rpc::rpc_pubsub_service::PubSubConfig,
};

impl FromClapArgMatches for PubSubConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(PubSubConfig {
            enable_block_subscription: matches.is_present("rpc_pubsub_enable_block_subscription"),
            enable_vote_subscription: matches.is_present("rpc_pubsub_enable_vote_subscription"),
            max_active_subscriptions: value_t!(
                matches,
                "rpc_pubsub_max_active_subscriptions",
                usize
            )?,
            queue_capacity_items: value_t!(matches, "rpc_pubsub_queue_capacity_items", usize)?,
            queue_capacity_bytes: value_t!(matches, "rpc_pubsub_queue_capacity_bytes", usize)?,
            worker_threads: value_t!(matches, "rpc_pubsub_worker_threads", usize)?,
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

    #[test]
    fn verify_args_struct_by_command_run_with_enable_vote_subscription() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            pub_sub_config: PubSubConfig {
                enable_vote_subscription: true,
                ..default_run_args.pub_sub_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-pubsub-enable-vote-subscription"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_max_active_subscriptions() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            pub_sub_config: PubSubConfig {
                max_active_subscriptions: 1000,
                ..default_run_args.pub_sub_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-pubsub-max-active-subscriptions", "1000"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_queue_capacity_items() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            pub_sub_config: PubSubConfig {
                queue_capacity_items: 9999,
                ..default_run_args.pub_sub_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-pubsub-queue-capacity-items", "9999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_queue_capacity_bytes() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            pub_sub_config: PubSubConfig {
                queue_capacity_bytes: 9999,
                ..default_run_args.pub_sub_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-pubsub-queue-capacity-bytes", "9999"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_worker_threads() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            pub_sub_config: PubSubConfig {
                worker_threads: 9999,
                ..default_run_args.pub_sub_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--rpc-pubsub-worker-threads", "9999"],
            expected_args,
        );
    }
}
