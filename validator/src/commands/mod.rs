pub mod authorized_voter;
pub mod contact_info;
pub mod exit;
pub mod monitor;
pub mod plugin;
pub mod repair_shred_from_peer;
pub mod repair_whitelist;
pub mod run;
pub mod set_identity;
pub mod set_log_filter;
pub mod set_public_address;
pub mod staked_nodes_overrides;
pub mod wait_for_restart_window;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("admin rpc error: {0}")]
    AdminRpc(#[from] jsonrpc_core_client::RpcError),

    #[error(transparent)]
    Clap(#[from] clap::Error),

    #[error(transparent)]
    Dynamic(#[from] Box<dyn std::error::Error>),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

pub trait FromClapArgMatches {
    fn from_clap_arg_match(matches: &clap::ArgMatches) -> Result<Self>
    where
        Self: Sized;
}

#[cfg(test)]
pub mod tests {
    use std::fmt::Debug;

    pub fn verify_args_struct_by_command<T>(app: clap::App, vec: Vec<&str>, expected_arg: T)
    where
        T: crate::commands::FromClapArgMatches + PartialEq + Debug,
    {
        let matches = app.get_matches_from(vec);
        let result = T::from_clap_arg_match(&matches);
        assert_eq!(result.unwrap(), expected_arg);
    }

    pub fn verify_args_struct_by_command_is_error<T>(app: clap::App, vec: Vec<&str>)
    where
        T: crate::commands::FromClapArgMatches + PartialEq + Debug,
    {
        let matches = app.get_matches_from_safe(vec);
        assert!(matches.is_err());
    }
}
