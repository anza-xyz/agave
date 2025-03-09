use {
    crate::{admin_rpc_service, commands::FromClapArgMatches},
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::{is_parsable, is_pubkey},
    solana_sdk::pubkey::Pubkey,
    std::path::Path,
};

const COMMAND: &str = "repair-shred-from-peer";

#[derive(Debug, PartialEq)]
pub struct RepairShredFromPeerArgs {
    pub pubkey: Option<Pubkey>,
    pub slot: u64,
    pub shred: u64,
}

impl FromClapArgMatches for RepairShredFromPeerArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self, String> {
        Ok(RepairShredFromPeerArgs {
            pubkey: value_t!(matches, "pubkey", Pubkey).ok(),
            slot: value_t!(matches, "slot", u64).map_err(|_| "slot is not a valid number")?,
            shred: value_t!(matches, "shred", u64).map_err(|_| "shred is not a valid number")?,
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
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

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    let RepairShredFromPeerArgs {
        pubkey,
        slot,
        shred,
    } = RepairShredFromPeerArgs::from_clap_arg_match(matches)?;

    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime()
        .block_on(async move {
            admin_client
                .await?
                .repair_shred_from_peer(pubkey, slot, shred)
                .await
        })
        .map_err(|err| format!("repair shred from peer request failed: {err}"))
}

#[cfg(test)]
mod tests {
    use {super::*, crate::commands::tests::verify_args_struct_by_command, std::str::FromStr};

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_missing_slot() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND]);
        let args = RepairShredFromPeerArgs::from_clap_arg_match(&matches);
        assert_eq!(args, Err("slot is not a valid number".to_string()));
    }

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_missing_shred() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND, "--slot", "1"]);
        let args = RepairShredFromPeerArgs::from_clap_arg_match(&matches);
        assert_eq!(args, Err("shred is not a valid number".to_string()));
    }

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_missing_pubkey() {
        verify_args_struct_by_command(
            command(),
            vec![COMMAND, "--slot", "1", "--shred", "2"],
            RepairShredFromPeerArgs {
                pubkey: None,
                slot: 1,
                shred: 2,
            },
        );
    }

    #[test]
    fn verify_args_struct_by_command_repair_shred_from_peer_with_pubkey() {
        verify_args_struct_by_command(
            command(),
            vec![
                COMMAND,
                "--slot",
                "1",
                "--shred",
                "2",
                "--pubkey",
                "ch1do11111111111111111111111111111111111111",
            ],
            RepairShredFromPeerArgs {
                pubkey: Some(
                    Pubkey::from_str("ch1do11111111111111111111111111111111111111").unwrap(),
                ),
                slot: 1,
                shred: 2,
            },
        );
    }
}
