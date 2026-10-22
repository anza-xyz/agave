use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{App, Arg, ArgMatches, SubCommand, value_t},
    std::path::Path,
};

const COMMAND: &str = "set-log-filter";

#[derive(Debug, PartialEq)]
pub struct SetLogFilterArgs {
    pub filter: String,
}

impl FromClapArgMatches for SetLogFilterArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(SetLogFilterArgs {
            filter: value_t!(matches, "filter", String)?,
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Adjust the validator log filter")
        .arg(
            Arg::with_name("filter")
                .takes_value(true)
                .index(1)
                .help("New filter using the same format as the RUST_LOG environment variable"),
        )
        .after_help("Note: the new filter only applies to the currently running validator instance")
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let set_log_filter_args = SetLogFilterArgs::from_clap_arg_match(matches)?;

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime().block_on(async move {
        admin_client
            .await?
            .set_log_filter(set_log_filter_args.filter)
            .await
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, crate::commands::tests::verify_args_struct_by_command};

    #[test]
    fn verify_args_struct_by_command_set_log_filter_default() {
        let matches = command().get_matches_from(vec![COMMAND]);
        assert!(SetLogFilterArgs::from_clap_arg_match(&matches).is_err());
    }

    #[test]
    fn verify_args_struct_by_command_set_log_filter_with_filter() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "expected_filter_value"],
            SetLogFilterArgs {
                filter: "expected_filter_value".to_string(),
            },
        );
    }
}
