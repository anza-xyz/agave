use {
    crate::commands::{FromClapArgMatches, Result},
    clap::ArgMatches,
    solana_rpc::rpc_pubsub_service::PubSubConfig,
};

impl FromClapArgMatches for PubSubConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(PubSubConfig {
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {}
