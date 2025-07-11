use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{value_t, ArgMatches},
    solana_rpc::rpc::JsonRpcConfig,
};

impl FromClapArgMatches for JsonRpcConfig {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(JsonRpcConfig {
            ..Default::default()
        })
    }
}
