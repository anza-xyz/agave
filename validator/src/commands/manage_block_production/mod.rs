use {
    crate::{
        admin_rpc_service,
        commands::{FromClapArgMatches, Result},
    },
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_core::{
        banking_stage::BankingStage,
        validator::{BlockProductionMethod, TransactionStructure},
    },
    std::{num::NonZeroUsize, path::Path},
};

const COMMAND: &str = "manage-block-production";

#[derive(Debug, PartialEq)]
pub struct ManageBlockProductionArgs {
    pub block_production_method: BlockProductionMethod,
    pub transaction_structure: TransactionStructure,
    pub num_workers: NonZeroUsize,
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
            num_workers: value_t!(matches, "block_production_num_workers", NonZeroUsize)
                .unwrap_or(BankingStage::default_num_workers()),
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Manage block production")
        .arg(
            Arg::with_name("block_production_method")
                .long("block-production-method")
                .alias("method")
                .value_name("METHOD")
                .takes_value(true)
                .possible_values(BlockProductionMethod::cli_names())
                .default_value(BlockProductionMethod::default().into())
                .help(BlockProductionMethod::cli_message()),
        )
        .arg(
            Arg::with_name("transaction_struct")
                .long("transaction-structure")
                .alias("struct")
                .value_name("STRUCT")
                .takes_value(true)
                .possible_values(TransactionStructure::cli_names())
                .default_value(TransactionStructure::default().into())
                .help(TransactionStructure::cli_message()),
        )
        .arg(
            Arg::with_name("block_production_num_workers")
                .long("block-production-num-workers")
                .alias("num-workers")
                .value_name("NUM")
                .takes_value(true)
                .help("Number of worker threads to use for block production"),
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<()> {
    let manage_block_production_args = ManageBlockProductionArgs::from_clap_arg_match(matches)?;

    println!(
        "Respawning block-production threads with method: {}, transaction structure: {} num_workers: {}",
        manage_block_production_args.block_production_method,
        manage_block_production_args.transaction_structure,
        manage_block_production_args.num_workers,
    );
    let admin_client = admin_rpc_service::connect(ledger_path);
    admin_rpc_service::runtime().block_on(async move {
        admin_client
            .await?
            .manage_block_production(
                manage_block_production_args.block_production_method,
                manage_block_production_args.transaction_structure,
                manage_block_production_args.num_workers,
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

        assert_eq!(
            args,
            ManageBlockProductionArgs {
                block_production_method: BlockProductionMethod::default(),
                transaction_structure: TransactionStructure::default(),
                num_workers: BankingStage::default_num_workers(),
            }
        );
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
            "--block-production-num-workers",
            "4",
        ]);
        let args = ManageBlockProductionArgs::from_clap_arg_match(&matches).unwrap();

        assert_eq!(
            args,
            ManageBlockProductionArgs {
                block_production_method: BlockProductionMethod::CentralScheduler,
                transaction_structure: TransactionStructure::Sdk,
                num_workers: NonZeroUsize::new(4).unwrap(),
            }
        );
    }
}
