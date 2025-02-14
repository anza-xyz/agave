use {
    crate::{admin_rpc_service, cli::DefaultArgs, commands::FromClapArgMatches},
    clap::{App, Arg, ArgMatches, SubCommand},
    std::{path::Path, process::exit},
};

const COMMAND: &str = "set-log-filter";

#[derive(Debug, PartialEq)]
pub struct SetLogFilterArg {
    pub filter: Option<String>,
}

impl FromClapArgMatches for SetLogFilterArg {
    fn from_clap_arg_match(matches: &ArgMatches) -> Self {
        SetLogFilterArg {
            filter: matches.value_of("filter").map(|s| s.to_string()),
        }
    }
}

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let set_log_filter_arg = SetLogFilterArg::from_clap_arg_match(matches);
    if set_log_filter_arg.filter.is_none() {
        eprintln!("filter is required");
        exit(1);
    }
    let filter = set_log_filter_arg.filter.unwrap();

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move { admin_client.await?.set_log_filter(filter).await })
        .unwrap_or_else(|err| {
            println!("set log filter failed: {err}");
            exit(1);
        });
}

#[cfg(test)]
mod tests {
    use {super::*, crate::commands::tests::verify_args_struct_by_command};

    #[test]
    fn verify_args_struct_by_command_set_log_filter_default() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND],
            SetLogFilterArg { filter: None },
        );
    }

    #[test]
    fn verify_args_struct_by_command_set_log_filter_with_filter() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND, "expected_filter_value"],
            SetLogFilterArg {
                filter: Some("expected_filter_value".to_string()),
            },
        );
    }
}
