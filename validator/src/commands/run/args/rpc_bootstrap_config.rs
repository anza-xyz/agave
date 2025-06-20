use {
    crate::commands::{FromClapArgMatches, Result},
    clap::ArgMatches,
};

#[derive(Debug, PartialEq, Clone, Default)]
pub struct RpcBootstrapConfig {
    pub no_genesis_fetch: bool,
    pub no_snapshot_fetch: bool,
}

impl FromClapArgMatches for RpcBootstrapConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let no_genesis_fetch = matches.is_present("no_genesis_fetch");

        let no_snapshot_fetch = matches.is_present("no_snapshot_fetch");

        Ok(Self {
            no_genesis_fetch,
            no_snapshot_fetch,
        })
    }
}
