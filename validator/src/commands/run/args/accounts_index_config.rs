use {
    crate::{
        cli::thread_args::{AccountsIndexFlushThreadsArg, ThreadArg},
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, values_t, ArgMatches},
    solana_accounts_db::accounts_index::{AccountsIndexConfig, IndexLimitMb},
    std::{num::NonZeroUsize, path::PathBuf},
};

impl FromClapArgMatches for AccountsIndexConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let num_flush_threads = value_t!(matches, AccountsIndexFlushThreadsArg::NAME, NonZeroUsize)
            .unwrap_or_else(|_| solana_accounts_db::accounts_index::default_num_flush_threads());

        let index_limit_mb = if !matches.is_present("enable_accounts_disk_index") {
            IndexLimitMb::InMemOnly
        } else {
            IndexLimitMb::Minimal
        };

        let accounts_index_paths = if matches.is_present("accounts_index_path") {
            values_t!(matches, "accounts_index_path", String)?
                .into_iter()
                .map(PathBuf::from)
                .collect()
        } else {
            vec![]
        };

        const MIB: usize = 1_024 * 1_024;
        let scan_results_limit_bytes =
            value_t!(matches, "accounts_index_scan_results_limit_mb", usize)
                .ok()
                .map(|mb| mb * MIB);

        Ok(AccountsIndexConfig {
            num_flush_threads: Some(num_flush_threads),
            bins: value_t!(matches, "accounts_index_bins", usize).ok(),
            index_limit_mb,
            drives: Some(accounts_index_paths),
            scan_results_limit_bytes,
            ..AccountsIndexConfig::default()
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
        solana_accounts_db::accounts_db::AccountsDbConfig,
    };

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_index_flush_threads() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                index: Some(AccountsIndexConfig {
                    num_flush_threads: Some(NonZeroUsize::new(2).unwrap()),
                    ..default_run_args.accounts_db_config.clone().index.unwrap()
                }),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-index-flush-threads", "2"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_index_bins() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                index: Some(AccountsIndexConfig {
                    bins: Some(512),
                    ..default_run_args.accounts_db_config.clone().index.unwrap()
                }),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-index-bins", "512"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_disable_accounts_disk_index() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                index: Some(AccountsIndexConfig {
                    index_limit_mb: IndexLimitMb::InMemOnly,
                    ..default_run_args.accounts_db_config.clone().index.unwrap()
                }),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--disable-accounts-disk-index"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_with_enable_accounts_disk_index() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                index: Some(AccountsIndexConfig {
                    index_limit_mb: IndexLimitMb::Minimal,
                    ..default_run_args.accounts_db_config.clone().index.unwrap()
                }),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--enable-accounts-disk-index"],
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_index_path() {
        // single path
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                accounts_db_config: AccountsDbConfig {
                    index: Some(AccountsIndexConfig {
                        drives: Some(vec![PathBuf::from("accounts_index_path_1")]),
                        index_limit_mb: IndexLimitMb::Minimal,
                        ..default_run_args.accounts_db_config.clone().index.unwrap()
                    }),
                    ..default_run_args.accounts_db_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--accounts-index-path",
                    "accounts_index_path_1",
                    "--enable-accounts-disk-index", // required by --accounts-index-path
                ],
                expected_args,
            );
        }

        // multiple paths
        {
            let default_run_args = crate::commands::run::args::RunArgs::default();
            let expected_args = RunArgs {
                accounts_db_config: AccountsDbConfig {
                    index: Some(AccountsIndexConfig {
                        drives: Some(vec![
                            PathBuf::from("accounts_index_path_1"),
                            PathBuf::from("accounts_index_path_2"),
                            PathBuf::from("accounts_index_path_3"),
                        ]),
                        index_limit_mb: IndexLimitMb::Minimal,
                        ..default_run_args.accounts_db_config.clone().index.unwrap()
                    }),
                    ..default_run_args.accounts_db_config.clone()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--accounts-index-path",
                    "accounts_index_path_1",
                    "--accounts-index-path",
                    "accounts_index_path_2",
                    "--accounts-index-path",
                    "accounts_index_path_3",
                    "--enable-accounts-disk-index", // required by --accounts-index-path
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_accounts_index_scan_results_limit_mb() {
        let default_run_args = crate::commands::run::args::RunArgs::default();
        let expected_args = RunArgs {
            accounts_db_config: AccountsDbConfig {
                index: Some(AccountsIndexConfig {
                    scan_results_limit_bytes: Some(2 * 1024 * 1024),
                    ..default_run_args.accounts_db_config.clone().index.unwrap()
                }),
                ..default_run_args.accounts_db_config.clone()
            },
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--accounts-index-scan-results-limit-mb", "2"],
            expected_args,
        );
    }
}
