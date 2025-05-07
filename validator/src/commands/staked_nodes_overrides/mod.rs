use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{App, Arg, ArgMatches, SubCommand},
    std::path::Path,
};

const COMMAND: &str = "staked-nodes-overrides";

#[derive(Debug, PartialEq)]
pub struct StakedNodesOverridesArgs {
    pub path: String,
}

impl FromClapArgMatches for StakedNodesOverridesArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(StakedNodesOverridesArgs {
            path: matches
                .value_of("path")
                .expect("path is required")
                .to_string(),
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let staked_nodes_overrides_args = StakedNodesOverridesArgs::from_clap_arg_match(matches)?;

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime().block_on(async move {
        admin_client
            .await?
            .set_staked_nodes_overrides(staked_nodes_overrides_args.path)
            .await
    })?;

    Ok(())
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
            command(),
            vec![COMMAND],
        );
    }

    #[test]
    fn verify_args_struct_by_command_staked_nodes_overrides_path() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "test.json"],
            StakedNodesOverridesArgs {
                path: "test.json".to_string(),
            },
        );
    }
}
