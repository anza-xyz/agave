use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{App, AppSettings, Arg, ArgMatches, SubCommand, value_t},
    solana_clap_utils::input_validators::is_parsable,
    solana_clock::Slot,
    std::path::Path,
};

const COMMAND: &str = "blockstore";

#[derive(Debug, PartialEq)]
#[cfg_attr(test, derive(Default))]
pub struct BlockstorePurgeArgs {
    pub maximum_purge_slot: Slot,
}

impl FromClapArgMatches for BlockstorePurgeArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(BlockstorePurgeArgs {
            maximum_purge_slot: value_t!(matches, "maximum_purge_slot", Slot)?,
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Interact with the validator's Blockstore")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::InferSubcommands)
        .setting(AppSettings::Hidden)
        .subcommand(
            SubCommand::with_name("purge")
                .about("Delete blocks from the Blockstore")
                .arg(
                    Arg::with_name("maximum_purge_slot")
                        .long("maximum-purge-slot")
                        .value_name("MAXIMUM_PURGE_SLOT")
                        .index(1)
                        .takes_value(true)
                        .required(true)
                        .validator(is_parsable::<Slot>)
                        .help(
                            "The maximum slot to purge from the Blockstore. All slots up to and \
                             including MAXIMUM_PURGE_SLOT will be purged",
                        ),
                ),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    match matches.subcommand() {
        ("purge", Some(subcommand_matches)) => {
            let BlockstorePurgeArgs { maximum_purge_slot } =
                BlockstorePurgeArgs::from_clap_arg_match(subcommand_matches)?;

            let admin_client = admin_rpc_service::connect(ledger_path);
            admin_rpc_service::runtime().block_on(async move {
                admin_client
                    .await?
                    .blockstore_purge(maximum_purge_slot)
                    .await
            })?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
