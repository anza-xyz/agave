use {
    crate::commands::{FromClapArgMatches, Result},
    clap::ArgMatches,
    solana_accounts_db::accounts_db::AccountsDbConfig,
};

impl FromClapArgMatches for AccountsDbConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(AccountsDbConfig {
            ..Default::default()
        })
    }
}
