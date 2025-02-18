use {
    crate::{admin_rpc_service, cli::DefaultArgs, commands::FromClapArgMatches},
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::{is_parsable, is_pubkey},
    solana_sdk::pubkey::Pubkey,
    std::{path::Path, process::exit},
};

const COMMAND: &str = "repair-shred-from-peer";

#[derive(Debug, PartialEq, Eq)]
pub struct RepairShredFromPeerArgs {
    pub pubkey: Option<Pubkey>,
    pub slot: Option<u64>,
    pub shred: Option<u64>,
}

impl FromClapArgMatches for RepairShredFromPeerArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Self {
        Self {
            pubkey: value_t!(matches, "pubkey", Pubkey).ok(),
            slot: value_t!(matches, "slot", u64).ok(),
            shred: value_t!(matches, "shred", u64).ok(),
        }
    }
}

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name(COMMAND)
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) {
    let repair_shred_from_peer_args = RepairShredFromPeerArgs::from_clap_arg_match(matches);
    if repair_shred_from_peer_args.slot.is_none() {
        eprintln!("slot is required");
        exit(1);
    }
    if repair_shred_from_peer_args.shred.is_none() {
        eprintln!("shred is required");
        exit(1);
    }

    let pubkey = repair_shred_from_peer_args.pubkey;
    let slot = repair_shred_from_peer_args.slot.unwrap();
    let shred = repair_shred_from_peer_args.shred.unwrap();

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move {
            admin_client
                .await?
                .repair_shred_from_peer(pubkey, slot, shred)
                .await
        })
        .unwrap_or_else(|err| {
            println!("repair shred from peer failed: {err}");
            exit(1);
        });
}

#[cfg(test)]
mod tests {
    use {super::*, crate::commands::tests::verify_args_struct_by_command, std::str::FromStr};

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_default() {
        let default_args = DefaultArgs::default();
        let app = command(&default_args);
        let matches = app.get_matches_from(vec![COMMAND]);
        let args = RepairShredFromPeerArgs::from_clap_arg_match(&matches);
        assert_eq!(
            args,
            RepairShredFromPeerArgs {
                pubkey: None,
                slot: None,
                shred: None,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_with_pubkey() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![
                COMMAND,
                "--pubkey",
                "ch1do11111111111111111111111111111111111111",
            ],
            RepairShredFromPeerArgs {
                pubkey: Some(
                    Pubkey::from_str("ch1do11111111111111111111111111111111111111").unwrap(),
                ),
                slot: None,
                shred: None,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_with_slot() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND, "--slot", "1"],
            RepairShredFromPeerArgs {
                pubkey: None,
                slot: Some(1),
                shred: None,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_with_shred() {
        verify_args_struct_by_command(
            command(&DefaultArgs::default()),
            vec![COMMAND, "--shred", "1"],
            RepairShredFromPeerArgs {
                pubkey: None,
                slot: None,
                shred: Some(1),
            },
        );
    }
}
