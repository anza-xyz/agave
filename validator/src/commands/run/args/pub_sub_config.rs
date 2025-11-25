#[cfg(test)]
use qualifier_attr::qualifiers;
use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, Arg, ArgMatches},
    solana_clap_utils::input_validators::is_parsable,
    solana_rayon_threadlimit::get_thread_count,
    solana_rpc::rpc_pubsub_service::PubSubConfig,
    std::{num::NonZeroUsize, sync::LazyLock},
};

static DEFAULT_RPC_PUBSUB_MAX_ACTIVE_SUBSCRIPTIONS: LazyLock<String> =
    LazyLock::new(|| PubSubConfig::default().max_active_subscriptions.to_string());
static DEFAULT_TEST_RPC_PUBSUB_MAX_ACTIVE_SUBSCRIPTIONS: LazyLock<String> = LazyLock::new(|| {
    PubSubConfig::default_for_tests()
        .max_active_subscriptions
        .to_string()
});

static DEFAULT_RPC_PUBSUB_QUEUE_CAPACITY_ITEMS: LazyLock<String> =
    LazyLock::new(|| PubSubConfig::default().queue_capacity_items.to_string());
static DEFAULT_TEST_RPC_PUBSUB_QUEUE_CAPACITY_ITEMS: LazyLock<String> = LazyLock::new(|| {
    PubSubConfig::default_for_tests()
        .queue_capacity_items
        .to_string()
});

static DEFAULT_RPC_PUBSUB_QUEUE_CAPACITY_BYTES: LazyLock<String> =
    LazyLock::new(|| PubSubConfig::default().queue_capacity_bytes.to_string());
static DEFAULT_TEST_RPC_PUBSUB_QUEUE_CAPACITY_BYTES: LazyLock<String> = LazyLock::new(|| {
    PubSubConfig::default_for_tests()
        .queue_capacity_bytes
        .to_string()
});

const DEFAULT_RPC_PUBSUB_WORKER_THREADS: &str = "4";
static DEFAULT_TEST_RPC_PUBSUB_WORKER_THREADS: LazyLock<String> =
    LazyLock::new(|| PubSubConfig::default_for_tests().worker_threads.to_string());

#[cfg_attr(test, qualifiers(pub(crate)))]
static DEFAULT_RPC_PUBSUB_NUM_NOTIFICATION_THREADS: LazyLock<String> =
    LazyLock::new(|| get_thread_count().to_string());

pub(crate) fn args<'a, 'b>(test_validator: bool) -> Vec<Arg<'a, 'b>> {
    let rpc_pubsub_notification_threads = Arg::with_name("rpc_pubsub_notification_threads")
        .long("rpc-pubsub-notification-threads")
        .takes_value(true)
        .value_name("NUM_THREADS")
        .validator(is_parsable::<usize>)
        .help(
            "The maximum number of threads that RPC PubSub will use for generating notifications. \
             0 will disable RPC PubSub notifications",
        );
    let (
        rpc_pubsub_notification_threads,
        default_rpc_pubsub_max_active_subscriptions,
        default_rpc_pubsub_queue_capacity_items,
        default_rpc_pubsub_queue_capacity_bytes,
    ) = if test_validator {
        (
            rpc_pubsub_notification_threads.default_value(&DEFAULT_TEST_RPC_PUBSUB_WORKER_THREADS),
            &DEFAULT_TEST_RPC_PUBSUB_MAX_ACTIVE_SUBSCRIPTIONS,
            &DEFAULT_TEST_RPC_PUBSUB_QUEUE_CAPACITY_ITEMS,
            &DEFAULT_RPC_PUBSUB_QUEUE_CAPACITY_BYTES,
        )
    } else {
        (
            rpc_pubsub_notification_threads
                .default_value_if(
                    "full_rpc_api",
                    None,
                    &DEFAULT_RPC_PUBSUB_NUM_NOTIFICATION_THREADS,
                )
                .requires("full_rpc_api"),
            &DEFAULT_RPC_PUBSUB_MAX_ACTIVE_SUBSCRIPTIONS,
            &DEFAULT_RPC_PUBSUB_QUEUE_CAPACITY_ITEMS,
            &DEFAULT_TEST_RPC_PUBSUB_QUEUE_CAPACITY_BYTES,
        )
    };

    vec![
        Arg::with_name("rpc_pubsub_enable_block_subscription")
            .long("rpc-pubsub-enable-block-subscription")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help("Enable the unstable RPC PubSub `blockSubscribe` subscription"),
        Arg::with_name("rpc_pubsub_enable_vote_subscription")
            .long("rpc-pubsub-enable-vote-subscription")
            .takes_value(false)
            .help("Enable the unstable RPC PubSub `voteSubscribe` subscription"),
        Arg::with_name("rpc_pubsub_max_active_subscriptions")
            .long("rpc-pubsub-max-active-subscriptions")
            .takes_value(true)
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .default_value(default_rpc_pubsub_max_active_subscriptions)
            .help(
                "The maximum number of active subscriptions that RPC PubSub will accept across \
                 all connections.",
            ),
        Arg::with_name("rpc_pubsub_queue_capacity_items")
            .long("rpc-pubsub-queue-capacity-items")
            .takes_value(true)
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .default_value(default_rpc_pubsub_queue_capacity_items)
            .help(
                "The maximum number of notifications that RPC PubSub will store across all \
                 connections.",
            ),
        Arg::with_name("rpc_pubsub_queue_capacity_bytes")
            .long("rpc-pubsub-queue-capacity-bytes")
            .takes_value(true)
            .value_name("BYTES")
            .validator(is_parsable::<usize>)
            .default_value(default_rpc_pubsub_queue_capacity_bytes)
            .help(
                "The maximum total size of notifications that RPC PubSub will store across all \
                 connections.",
            ),
        Arg::with_name("rpc_pubsub_worker_threads")
            .long("rpc-pubsub-worker-threads")
            .takes_value(true)
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .default_value(DEFAULT_RPC_PUBSUB_WORKER_THREADS)
            .help("PubSub worker threads"),
        rpc_pubsub_notification_threads,
    ]
}

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
            notification_threads: value_t!(matches, "rpc_pubsub_notification_threads", usize)
                .ok()
                .and_then(NonZeroUsize::new),
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

    #[test]
    fn verify_args_struct_by_command_run_with_notification_threads() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            json_rpc_config: JsonRpcConfig {
                full_api: true,
                ..default_run_args.json_rpc_config.clone()
            },
            pub_sub_config: PubSubConfig {
                notification_threads: Some(NonZeroUsize::new(9999).unwrap()),
                ..default_run_args.pub_sub_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec![
                "--full-rpc-api", // required by --rpc-pubsub-notification-threads
                "--rpc-pubsub-notification-threads",
                "9999",
            ],
            expected_args,
        );
    }

    #[test]
    fn test_default_rpc_pubsub_max_active_subscriptions_unchanged() {
        assert_eq!(
            *DEFAULT_RPC_PUBSUB_MAX_ACTIVE_SUBSCRIPTIONS,
            1_000_000.to_string()
        );
    }

    #[test]
    fn test_default_rpc_pubsub_queue_capacity_items_unchanged() {
        assert_eq!(
            *DEFAULT_RPC_PUBSUB_QUEUE_CAPACITY_ITEMS,
            10_000_000.to_string()
        );
    }

    #[test]
    fn test_default_rpc_pubsub_queue_capacity_bytes_unchanged() {
        assert_eq!(
            *DEFAULT_RPC_PUBSUB_QUEUE_CAPACITY_BYTES,
            (256 * 1024 * 1024).to_string()
        );
    }

    #[test]
    fn test_default_rpc_pubsub_worker_threads_unchanged() {
        assert_eq!(DEFAULT_RPC_PUBSUB_WORKER_THREADS, "4");
    }
}
