use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{App, Arg, ArgMatches, SubCommand},
    std::{path::Path, process::exit},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
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
            "Note: the new staked nodes overrides only applies to the currently running validator \
             instance",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    if !matches.is_present("path") {
        println!("staked-nodes-overrides requires argument of location of the configuration");
        exit(1);
    }

    let path = matches.value_of("path").unwrap();

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move {
            admin_client
                .await?
                .set_staked_nodes_overrides(path.to_string())
                .await
        })
        .unwrap_or_else(|err| {
            println!("setStakedNodesOverrides request failed: {err}");
            exit(1);
        });
}
