use {
    crate::commands::{FromClapArgMatches, Result},
    clap::ArgMatches,
    solana_accounts_db::{accounts_db::AccountsDbConfig, accounts_index::AccountsIndexConfig},
};

impl FromClapArgMatches for AccountsDbConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let accounts_index_config = AccountsIndexConfig::from_clap_arg_match(matches)?;

        Ok(AccountsDbConfig {
            index: Some(accounts_index_config),
            ..Default::default()
        })
    }
}
