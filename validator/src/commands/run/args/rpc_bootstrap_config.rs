use {
    crate::{
        bootstrap::RpcBootstrapConfig,
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, Arg, ArgMatches},
    solana_genesis_utils::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
    std::sync::LazyLock,
};

static DEFAULT_MAX_GENESIS_ARCHIVE_UNPACKED_SIZE: LazyLock<String> =
    LazyLock::new(|| MAX_GENESIS_ARCHIVE_UNPACKED_SIZE.to_string());

#[cfg(test)]
impl Default for RpcBootstrapConfig {
    fn default() -> Self {
        Self {
            no_genesis_fetch: false,
            no_snapshot_fetch: false,
            check_vote_account: None,
            only_known_rpc: false,
            max_genesis_archive_unpacked_size: 10485760,
            incremental_snapshot_fetch: true,
        }
    }
}

impl FromClapArgMatches for RpcBootstrapConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let no_genesis_fetch = matches.is_present("no_genesis_fetch");

        let no_snapshot_fetch = matches.is_present("no_snapshot_fetch");

        let check_vote_account = matches
            .value_of("check_vote_account")
            .map(|url| url.to_string());

        let only_known_rpc = matches.is_present("only_known_rpc");

        let max_genesis_archive_unpacked_size =
            value_t!(matches, "max_genesis_archive_unpacked_size", u64).map_err(|err| {
                Box::<dyn std::error::Error>::from(format!(
                    "failed to parse max_genesis_archive_unpacked_size: {err}"
                ))
            })?;

        let no_incremental_snapshots = matches.is_present("no_incremental_snapshots");

        Ok(Self {
            no_genesis_fetch,
            no_snapshot_fetch,
            check_vote_account,
            only_known_rpc,
            max_genesis_archive_unpacked_size,
            incremental_snapshot_fetch: !no_incremental_snapshots,
        })
    }
}

pub(crate) fn args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::with_name("no_genesis_fetch")
            .long("no-genesis-fetch")
            .takes_value(false)
            .help("Do not fetch genesis from the cluster"),
        Arg::with_name("no_snapshot_fetch")
            .long("no-snapshot-fetch")
            .takes_value(false)
            .help(
                "Do not attempt to fetch a snapshot from the cluster, start from a local snapshot \
                 if present",
            ),
        Arg::with_name("check_vote_account")
            .long("check-vote-account")
            .takes_value(true)
            .value_name("RPC_URL")
            .requires("entrypoint")
            .conflicts_with_all(&["no_voting"])
            .help(
                "Sanity check vote account state at startup. The JSON RPC endpoint at RPC_URL \
                 must expose `--full-rpc-api`",
            ),
        Arg::with_name("only_known_rpc")
            .alias("no-untrusted-rpc")
            .long("only-known-rpc")
            .takes_value(false)
            .requires("known_validators")
            .help("Use the RPC service of known validators only"),
        Arg::with_name("max_genesis_archive_unpacked_size")
            .long("max-genesis-archive-unpacked-size")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&DEFAULT_MAX_GENESIS_ARCHIVE_UNPACKED_SIZE)
            .help("maximum total uncompressed file size of downloaded genesis archive"),
        Arg::with_name("no_incremental_snapshots")
            .long("no-incremental-snapshots")
            .takes_value(false)
            .help("Disable incremental snapshots"),
    ]
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::run::args::{
            tests::verify_args_struct_by_command_run_with_identity_setup, RunArgs,
        },
        solana_pubkey::Pubkey,
        std::{
            collections::HashSet,
            net::{IpAddr, Ipv4Addr, SocketAddr},
        },
    };

    #[test]
    fn verify_args_struct_by_command_run_with_no_genesis_fetch() {
        // long arg
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                rpc_bootstrap_config: RpcBootstrapConfig {
                    no_genesis_fetch: true,
                    ..RpcBootstrapConfig::default()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec!["--no-genesis-fetch"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_no_snapshot_fetch() {
        // long arg
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                rpc_bootstrap_config: RpcBootstrapConfig {
                    no_snapshot_fetch: true,
                    ..RpcBootstrapConfig::default()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec!["--no-snapshot-fetch"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_check_vote_account() {
        // long arg
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                entrypoints: vec![SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    8000,
                )],
                rpc_bootstrap_config: RpcBootstrapConfig {
                    check_vote_account: Some("https://api.mainnet-beta.solana.com".to_string()),
                    ..RpcBootstrapConfig::default()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    // required by --check-vote-account
                    "--entrypoint",
                    "127.0.0.1:8000",
                    "--check-vote-account",
                    "https://api.mainnet-beta.solana.com",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_only_known_rpc() {
        // long arg
        {
            let default_run_args = RunArgs::default();
            let known_validators_pubkey = Pubkey::new_unique();
            let known_validators = Some(HashSet::from([known_validators_pubkey]));
            let expected_args = RunArgs {
                known_validators,
                rpc_bootstrap_config: RpcBootstrapConfig {
                    only_known_rpc: true,
                    ..RpcBootstrapConfig::default()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    // required by --only-known-rpc
                    "--known-validator",
                    &known_validators_pubkey.to_string(),
                    "--only-known-rpc",
                ],
                expected_args,
            );
        }

        // alias
        {
            let default_run_args = RunArgs::default();
            let known_validators_pubkey = Pubkey::new_unique();
            let known_validators = Some(HashSet::from([known_validators_pubkey]));
            let expected_args = RunArgs {
                known_validators,
                rpc_bootstrap_config: RpcBootstrapConfig {
                    only_known_rpc: true,
                    ..RpcBootstrapConfig::default()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    // required by --no-untrusted-rpc
                    "--known-validator",
                    &known_validators_pubkey.to_string(),
                    "--no-untrusted-rpc",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_incremental_snapshot_fetch() {
        // long arg
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                rpc_bootstrap_config: RpcBootstrapConfig {
                    incremental_snapshot_fetch: false,
                    ..RpcBootstrapConfig::default()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--no-incremental-snapshots"],
                expected_args,
            );
        }
    }

    #[test]
    fn test_default_max_genesis_archive_unpacked_size_unchanged() {
        assert_eq!(
            *DEFAULT_MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            (10 * 1024 * 1024).to_string()
        );
    }
}
