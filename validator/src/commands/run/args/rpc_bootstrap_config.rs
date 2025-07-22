use {
    crate::{
        bootstrap::RpcBootstrapConfig,
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, ArgMatches},
};

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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::run::args::{
            tests::verify_args_struct_by_command_run_with_identity_setup, RunArgs,
        },
        std::net::{IpAddr, Ipv4Addr, SocketAddr},
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
                    // entrypoint is required for check-vote-account
                    "--entrypoint",
                    "127.0.0.1:8000",
                    "--check-vote-account",
                    "https://api.mainnet-beta.solana.com",
                ],
                expected_args,
            );
        }
    }
}
