use {
    crate::{admin_rpc_service, cli::DefaultArgs, commands::FromClapArgMatches},
    clap::{App, Arg, ArgMatches, SubCommand},
    std::{path::Path, process::exit},
};

const COMMAND: &str = "staked-nodes-overrides";

#[derive(Debug, PartialEq)]
pub struct StakedNodesOverridesArgs {
    pub path: String,
}

impl FromClapArgMatches for StakedNodesOverridesArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Self {
        StakedNodesOverridesArgs {
            path: matches.value_of("path").unwrap().to_string(),
        }
    }
}

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name(COMMAND)
        .about("Overrides stakes of specific node identities.")
        .arg(
            Arg::with_name("path")
                .value_name("PATH")
                .takes_value(true)
                .required(true)
                .help(
                    "Provide path to a file with custom overrides for stakes of specific validator identities.",
                ),
        )
        .after_help(
            "Note: the new staked nodes overrides only applies to the currently running validator instance",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let staked_nodes_overrides_args = StakedNodesOverridesArgs::from_clap_arg_match(matches);
    if staked_nodes_overrides_args.path.is_empty() {
        println!("staked-nodes-overrides requires argument of location of the configuration");
        exit(1);
    }

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move {
            admin_client
                .await?
                .set_staked_nodes_overrides(staked_nodes_overrides_args.path)
                .await
        })
        .unwrap_or_else(|err| {
            println!("setStakedNodesOverrides request failed: {err}");
            exit(1);
        });
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::commands::tests::{
            verify_args_struct_by_command, verify_args_struct_by_command_is_error,
        },
    };

    #[test]
    fn verify_args_struct_by_command_staked_nodes_overrides_default() {
        verify_args_struct_by_command_is_error::<StakedNodesOverridesArgs>(
            command(&DefaultArgs::default()),
            vec![COMMAND],
        );
    }

    #[test]
    fn verify_args_struct_by_command_staked_nodes_overrides_path() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND, "test.json"],
            StakedNodesOverridesArgs {
                path: "test.json".to_string(),
            },
        );
    }
}
