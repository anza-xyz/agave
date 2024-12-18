use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{value_t_or_exit, App, Arg, ArgMatches, SubCommand},
    std::{path::PathBuf, process::exit},
};

pub struct SetLogFilterArg {
    pub filter: String,
}

impl SetLogFilterArg {
    pub fn new(matches: &ArgMatches) -> Self {
        SetLogFilterArg {
            filter: value_t_or_exit!(matches, "filter", String),
        }
    }
}

pub fn command<'a>(_default_args: &'a DefaultArgs) -> App<'a, 'a> {
    SubCommand::with_name("set-log-filter")
        .about("Adjust the validator log filter")
        .arg(
            Arg::with_name("filter")
                .takes_value(true)
                .index(1)
                .help("New filter using the same format as the RUST_LOG environment variable"),
        )
        .after_help("Note: the new filter only applies to the currently running validator instance")
}

pub fn execute(matches: &ArgMatches, ledger_path: &PathBuf) {
    let set_log_filter_arg = SetLogFilterArg::new(matches);

    let admin_client = admin_rpc_service::connect(&ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move {
            admin_client
                .await?
                .set_log_filter(set_log_filter_arg.filter)
                .await
        })
        .unwrap_or_else(|err| {
            println!("set log filter failed: {err}");
            exit(1);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_log_filter_arg() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from(vec!["set-log-filter", "test"]);
        let arg = SetLogFilterArg::new(&matches);
        assert_eq!(arg.filter, "test");
    }
}
