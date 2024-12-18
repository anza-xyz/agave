use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{value_t, value_t_or_exit, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::{is_parsable, is_pubkey},
    solana_sdk::pubkey::Pubkey,
    std::{path::PathBuf, process::exit},
};

pub fn command<'a>(_default_args: &'a DefaultArgs) -> App<'a, 'a> {
    SubCommand::with_name("repair-shred-from-peer")
        .about("Request a repair from the specified validator")
        .arg(
            Arg::with_name("pubkey")
                .long("pubkey")
                .value_name("PUBKEY")
                .required(false)
                .takes_value(true)
                .validator(is_pubkey)
                .help("Identity pubkey of the validator to repair from"),
        )
        .arg(
            Arg::with_name("slot")
                .long("slot")
                .value_name("SLOT")
                .takes_value(true)
                .validator(is_parsable::<u64>)
                .help("Slot to repair"),
        )
        .arg(
            Arg::with_name("shred")
                .long("shred")
                .value_name("SHRED")
                .takes_value(true)
                .validator(is_parsable::<u64>)
                .help("Shred to repair"),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &PathBuf) {
    let pubkey = value_t!(matches, "pubkey", Pubkey).ok();
    let slot = value_t_or_exit!(matches, "slot", u64);
    let shred_index = value_t_or_exit!(matches, "shred", u64);
    let admin_client = admin_rpc_service::connect(&ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move {
            admin_client
                .await?
                .repair_shred_from_peer(pubkey, slot, shred_index)
                .await
        })
        .unwrap_or_else(|err| {
            println!("repair shred from peer failed: {err}");
            exit(1);
        });
}
