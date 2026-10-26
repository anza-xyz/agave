use {
    crate::{admin_rpc_service, commands::Result},
    clap::{App, Arg, ArgMatches, SubCommand, value_t},
    std::path::Path,
};

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name("set-event-filter")
        .about("Adjust the validator event filter")
        .arg(
            Arg::with_name("filter")
                .required(true)
                .takes_value(true)
                .index(1)
                .help(
                    "New filter using the AGAVE_EVENTS format: on, off, or comma-separated stream \
                     prefixes with optional =on or =off rules",
                ),
        )
        .after_help("Note: the new filter only applies to the currently running validator instance")
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let filter = value_t!(matches, "filter", String)?;
    admin_rpc_service::runtime().block_on(async move {
        admin_rpc_service::connect(ledger_path)
            .await?
            .set_event_filter(filter)
            .await
    })?;
    Ok(())
}
