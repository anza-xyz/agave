#![allow(clippy::arithmetic_side_effects)]
#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
use jemallocator::Jemalloc;
use {
    agave_validator::{
        cli::{app, warn_for_deprecated_arguments, DefaultArgs},
        commands,
        config_file::ConfigFile,
    },
    clap::{ArgMatches, Error},
    config::Config,
    log::error,
    std::{fs, path::PathBuf, process::exit},
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

pub fn main() {
    let default_args = DefaultArgs::new();
    let solana_version = solana_version::version!();
    let cli_app = app(solana_version, &default_args);
    let matches = cli_app.get_matches();
    warn_for_deprecated_arguments(&matches);

    let config = match load_config(&matches) {
        Ok(config) => config,
        Err(e) => {
            e.exit();
        }
    };

    let ledger_path = PathBuf::from(matches.value_of("ledger_path").unwrap());

    match matches.subcommand() {
        ("init", _) => commands::run::execute(
            &matches,
            solana_version,
            commands::run::execute::Operation::Initialize,
            &config,
        )
        .inspect_err(|err| error!("Failed to initialize validator: {err}"))
        .map_err(commands::Error::Dynamic),
        ("", _) | ("run", _) => commands::run::execute(
            &matches,
            solana_version,
            commands::run::execute::Operation::Run,
            &config,
        )
        .inspect_err(|err| error!("Failed to start validator: {err}"))
        .map_err(commands::Error::Dynamic),
        ("authorized-voter", Some(authorized_voter_subcommand_matches)) => {
            commands::authorized_voter::execute(authorized_voter_subcommand_matches, &ledger_path)
        }
        ("plugin", Some(plugin_subcommand_matches)) => {
            commands::plugin::execute(plugin_subcommand_matches, &ledger_path)
        }
        ("contact-info", Some(subcommand_matches)) => {
            commands::contact_info::execute(subcommand_matches, &ledger_path)
        }
        ("exit", Some(subcommand_matches)) => {
            commands::exit::execute(subcommand_matches, &ledger_path)
        }
        ("monitor", _) => commands::monitor::execute(&matches, &ledger_path),
        ("staked-nodes-overrides", Some(subcommand_matches)) => {
            commands::staked_nodes_overrides::execute(subcommand_matches, &ledger_path)
        }
        ("set-identity", Some(subcommand_matches)) => {
            commands::set_identity::execute(subcommand_matches, &ledger_path)
        }
        ("set-log-filter", Some(subcommand_matches)) => {
            commands::set_log_filter::execute(subcommand_matches, &ledger_path)
        }
        ("wait-for-restart-window", Some(subcommand_matches)) => {
            commands::wait_for_restart_window::execute(subcommand_matches, &ledger_path)
        }
        ("repair-shred-from-peer", Some(subcommand_matches)) => {
            commands::repair_shred_from_peer::execute(subcommand_matches, &ledger_path)
        }
        ("repair-whitelist", Some(repair_whitelist_subcommand_matches)) => {
            commands::repair_whitelist::execute(repair_whitelist_subcommand_matches, &ledger_path)
        }
        ("set-public-address", Some(subcommand_matches)) => {
            commands::set_public_address::execute(subcommand_matches, &ledger_path)
        }
        ("manage-block-production", Some(subcommand_matches)) => {
            commands::manage_block_production::execute(subcommand_matches, &ledger_path)
        }
        _ => unreachable!(),
    }
    .unwrap_or_else(|err| {
        println!("Validator command failed: {err}");
        exit(1);
    })
}

fn load_config(arg_matches: &ArgMatches) -> Result<ConfigFile, Error> {
    let Some(config_path) = arg_matches.values_of("config") else {
        return Ok(ConfigFile::default());
    };

    let mut config_builder = Config::builder();

    for config_path in config_path {
        let io_err = |e| {
            Error::value_validation_auto(format!(
                "Failed to read config file at {config_path}: {e}"
            ))
        };

        let metadata = fs::metadata(config_path).map_err(io_err)?;

        match metadata {
            metadata if metadata.is_dir() => {
                for entry in fs::read_dir(config_path).map_err(io_err)? {
                    let entry = entry.map_err(io_err)?;
                    let path = entry.path();
                    if path.is_file() {
                        let path = entry.path().into_os_string();
                        let path = path.to_string_lossy();
                        config_builder = config_builder.add_source(config::File::with_name(&path));
                    }
                }
            }
            metadata if metadata.is_file() => {
                config_builder = config_builder.add_source(config::File::with_name(config_path));
            }
            _ => {
                return Err(Error::value_validation_auto(format!(
                    "Config file is not a directory or file: {config_path}"
                )));
            }
        }
    }

    config_builder
        .build()
        .and_then(|c| c.try_deserialize())
        .map_err(|e| Error::value_validation_auto(format!("Failed to deserialize config: {e}")))
}
