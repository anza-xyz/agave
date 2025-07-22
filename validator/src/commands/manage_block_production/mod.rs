use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_core::validator::{BlockProductionMethod, TransactionStructure},
    std::path::Path,
};

const COMMAND: &str = "manage-block-production";

#[derive(Debug, PartialEq)]
#[cfg_attr(test, derive(Default))]
pub struct ManageBlockProductionArgs {
    pub block_production_method: BlockProductionMethod,
    pub transaction_structure: TransactionStructure,
}

impl FromClapArgMatches for ManageBlockProductionArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(ManageBlockProductionArgs {
            block_production_method: value_t!(
                matches,
                "block_production_method",
                BlockProductionMethod
            )
            .unwrap_or_default(),
            transaction_structure: value_t!(matches, "transaction_struct", TransactionStructure)
                .unwrap_or_default(),
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Manage block production")
        .arg(
            Arg::with_name("block_production_method")
                .long("block-production-method")
                .value_name("METHOD")
                .takes_value(true)
                .possible_values(BlockProductionMethod::cli_names())
                .default_value(BlockProductionMethod::default().into())
                .help(BlockProductionMethod::cli_message()),
        )
        .arg(
            Arg::with_name("transaction_struct")
                .long("transaction-structure")
                .value_name("STRUCT")
                .takes_value(true)
                .possible_values(TransactionStructure::cli_names())
                .default_value(TransactionStructure::default().into())
                .help(TransactionStructure::cli_message()),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let manage_block_production_args = ManageBlockProductionArgs::from_clap_arg_match(matches)?;

    println!(
        "Respawning block-production threads with method: {}, transaction structure: {}",
        manage_block_production_args.block_production_method,
        manage_block_production_args.transaction_structure
    );
    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime().block_on(async move {
        admin_client
            .await?
            .manage_block_production(
                manage_block_production_args.block_production_method,
                manage_block_production_args.transaction_structure,
            )
            .await
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_args_struct_by_command_manage_block_production_default() {
        let app = command();
        let matches = app.get_matches_from(vec![COMMAND]);
        let args = ManageBlockProductionArgs::from_clap_arg_match(&matches).unwrap();

        assert_eq!(args, ManageBlockProductionArgs::default());
    }

    #[test]
    fn verify_args_struct_by_command_manage_block_production_with_args() {
        let app = command();
        let matches = app.get_matches_from(vec![
            COMMAND,
            "--block-production-method",
            "central-scheduler",
            "--transaction-structure",
            "sdk",
        ]);
        println!("{:?}", matches);
        let args = ManageBlockProductionArgs::from_clap_arg_match(&matches).unwrap();

        assert_eq!(
            args,
            ManageBlockProductionArgs {
                block_production_method: BlockProductionMethod::CentralScheduler,
                transaction_structure: TransactionStructure::Sdk,
            }
        );
    }
}
