use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{App, Arg, ArgMatches, SubCommand},
    std::{path::PathBuf, process::exit},
};

pub struct StakedNodesOverridesArg {
    pub path: String,
}

impl StakedNodesOverridesArg {
    pub fn new(matches: &ArgMatches) -> Self {
        StakedNodesOverridesArg {
            path: matches.value_of("path").unwrap().to_string(),
        }
    }
}

pub fn command<'a>(_default_args: &'a DefaultArgs) -> App<'a, 'a> {
    SubCommand::with_name("staked-nodes-overrides")
        .about("Overrides stakes of specific node identities.")
        .arg(
            Arg::with_name("path")
                .value_name("PATH")
                .takes_value(true)
                .required(true)
                .help(
                    "Provide path to a file with custom overrides for stakes of specific \
               validator identities.",
                ),
        )
        .after_help(
            "Note: the new staked nodes overrides only applies to the currently running \
       validator instance",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &PathBuf) {
    let staked_nodes_overrides_arg = StakedNodesOverridesArg::new(matches);

    let admin_client = admin_rpc_service::connect(&ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move {
            admin_client
                .await?
                .set_staked_nodes_overrides(staked_nodes_overrides_arg.path)
                .await
        })
        .unwrap_or_else(|err| {
            println!("setStakedNodesOverrides request failed: {err}");
            exit(1);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_staked_nodes_overrides_arg() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from(vec!["staked-nodes-overrides", "test.json"]);
        let arg = StakedNodesOverridesArg::new(&matches);
        assert_eq!(arg.path, "test.json");
    }
}
