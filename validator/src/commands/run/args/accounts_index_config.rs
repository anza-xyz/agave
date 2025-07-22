use {
    crate::{
        cli::thread_args::{AccountsIndexFlushThreadsArg, ThreadArg},
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, ArgMatches},
    solana_accounts_db::accounts_index::AccountsIndexConfig,
    std::num::NonZeroUsize,
};

impl FromClapArgMatches for AccountsIndexConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let num_flush_threads = value_t!(matches, AccountsIndexFlushThreadsArg::NAME, NonZeroUsize)
            .unwrap_or_else(|_| solana_accounts_db::accounts_index::default_num_flush_threads());

        Ok(AccountsIndexConfig {
            num_flush_threads: Some(num_flush_threads),
            bins: value_t!(matches, "accounts_index_bins", usize).ok(),
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
}
