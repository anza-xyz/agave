use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
};

#[derive(Debug, PartialEq, Clone)]
pub struct RpcBootstrapConfig {
    pub no_genesis_fetch: bool,
    pub no_snapshot_fetch: bool,
    pub only_known_rpc: bool,
    pub max_genesis_archive_unpacked_size: u64,
    pub check_vote_account: Option<String>,
    pub incremental_snapshot_fetch: bool,
}

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

        let incremental_snapshot_fetch = !matches.is_present("no_incremental_snapshots");

        Ok(Self {
            no_genesis_fetch,
            no_snapshot_fetch,
            check_vote_account,
            only_known_rpc,
            max_genesis_archive_unpacked_size,
            incremental_snapshot_fetch,
        })
    }
}
