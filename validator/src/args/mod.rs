use {
    crate::config_file::ValidatorConfig,
    clap::{App, Error},
};

pub type Result<T> = std::result::Result<T, Error>;

pub trait ClapAppExt<'a, 'b> {
    fn layered_arg<T: LayeredArg>(self) -> Self;
}

impl<'a, 'b> ClapAppExt<'a, 'b> for App<'a, 'b> {
    fn layered_arg<T: LayeredArg>(self) -> Self {
        T::declare_cli_args(self)
    }
}

/// Standardized layered argument scheme.
///
/// This trait can be used to configure so called "layered" arguments.
/// Layered arguments are arguments whose values may be provided via multiple sources
/// with a particular precedence.
///
/// This trait exists to provide a standardized protocol for configuring layered arguments.
pub trait LayeredArg {
    /// Declare the CLI arguments, given an [`App`].
    ///
    /// Can be used to specify the clap configuration, help text, etc for a given argument.
    fn declare_cli_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
        app
    }

    /// Parse the argument from the validator configuration file.
    fn from_validator_config(_config: &ValidatorConfig) -> Option<Result<Self>>
    where
        Self: Sized,
    {
        None
    }

    /// Parse the argument from the CLI.
    fn from_clap_arg_matches(_matches: &clap::ArgMatches) -> Option<Result<Self>>
    where
        Self: Sized,
    {
        None
    }

    /// Parse the argument from environment variables.
    fn from_env() -> Option<Result<Self>>
    where
        Self: Sized,
    {
        None
    }

    /// Parse the argument from the configured sources.
    ///
    /// Precedence is as follows:
    /// 1. CLI arguments
    /// 2. Environment variables
    /// 3. Configuration file
    fn parse(matches: &clap::ArgMatches, config: &ValidatorConfig) -> Option<Result<Self>>
    where
        Self: Sized,
    {
        Self::from_clap_arg_matches(matches)
            .or_else(|| Self::from_env())
            .or_else(|| Self::from_validator_config(config))
    }
}

pub mod xdp;
