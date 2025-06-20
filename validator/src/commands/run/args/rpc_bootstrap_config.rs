use {
    crate::commands::{FromClapArgMatches, Result},
    clap::ArgMatches,
};

#[derive(Debug, PartialEq, Clone, Default)]
pub struct RpcBootstrapConfig {
    pub no_genesis_fetch: bool,
    pub no_snapshot_fetch: bool,
    pub check_vote_account: Option<String>,
    pub only_known_rpc: bool,
}

impl FromClapArgMatches for RpcBootstrapConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let no_genesis_fetch = matches.is_present("no_genesis_fetch");

        let no_snapshot_fetch = matches.is_present("no_snapshot_fetch");

        let check_vote_account = matches
            .value_of("check_vote_account")
            .map(|url| url.to_string());

        let only_known_rpc = matches.is_present("only_known_rpc");

        Ok(Self {
            no_genesis_fetch,
            no_snapshot_fetch,
            check_vote_account,
            only_known_rpc,
        })
    }
}
