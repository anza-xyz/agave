#![allow(clippy::arithmetic_side_effects)]
#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
use jemallocator::Jemalloc;
use {
    agave_validator::{
        cli::{app, warn_for_deprecated_arguments, DefaultArgs},
        commands,
        config::ValidatorConfig,
    },
    clap::ArgMatches,
    log::{error, info, warn},
    std::{env, path::PathBuf, process::exit},
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

pub fn main() {
    let default_args = DefaultArgs::new();
    let solana_version = solana_version::version!();
    

    apply_config_defaults();
    
    let cli_app = app(solana_version, &default_args);
    let matches = cli_app.get_matches();
    warn_for_deprecated_arguments(&matches);
    emit_config_conflict_warnings(&matches);


    let ledger_path = matches
        .value_of("ledger_path")
        .map(PathBuf::from)
        .or_else(|| env::var("AGAVE_LEDGER").ok().map(PathBuf::from))
        .unwrap_or_else(|| {
            eprintln!("error: --ledger is required (or set AGAVE_LEDGER env var)");
            eprintln!("Tip: export AGAVE_LEDGER=/path/to/ledger");
            exit(1);
        });

    let result = match matches.subcommand() {
        ("init", Some(subcommand_matches)) => commands::run::execute::execute(
            subcommand_matches,
            solana_version,
            commands::run::execute::Operation::Initialize,
        )
        .inspect_err(|err| error!("Failed to initialize validator: {err}"))
        .map_err(commands::Error::Dynamic),
        ("", None) | ("run", Some(_)) => commands::run::execute::execute(
            &matches,
            solana_version,
            commands::run::execute::Operation::Run,
        )
        .inspect_err(|err| error!("Failed to start validator: {err}"))
        .map_err(commands::Error::Dynamic),
        _ => {
            // Handle other subcommands
            let subcommand_result = match matches.subcommand() {
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
                _ => Ok(()),
            };
            subcommand_result.map_err(|e| commands::Error::Dynamic(Box::new(e)))
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        exit(1);
    }
}


fn apply_config_defaults() {

    let args: Vec<String> = env::args().collect();
    let config_path = args.windows(2)
        .find(|pair| pair[0] == "--config")
        .map(|pair| &pair[1]);
    
    let Some(path) = config_path else {
        return; // No config file specified
    };
    
    let config = match ValidatorConfig::load_from_file(path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("Config error: {err}");
            exit(1);
        }
    };
    
    let mut applied = 0usize;
    if let Some(ledger) = config.ledger {
        if env::var("AGAVE_LEDGER").is_err() {
            env::set_var("AGAVE_LEDGER", ledger);
            applied += 1;
        }
    }
    if let Some(identity) = config.identity {
        if env::var("AGAVE_IDENTITY").is_err() {
            env::set_var("AGAVE_IDENTITY", identity);
            applied += 1;
        }
    }
    if let Some(entrypoints) = config.entrypoint {
        if env::var("AGAVE_ENTRYPOINT").is_err() {
            env::set_var("AGAVE_ENTRYPOINT", entrypoints.join(","));
            applied += 1;
        }
    }
    if let Some(log_path) = config.log {
        if env::var("AGAVE_LOG").is_err() {
            env::set_var("AGAVE_LOG", log_path);
            applied += 1;
        }
    }
    if let Some(rpc_port) = config.rpc_port {
        if env::var("AGAVE_RPC_PORT").is_err() {
            env::set_var("AGAVE_RPC_PORT", rpc_port.to_string());
            applied += 1;
        }
    }
    if let Some(kv) = config.known_validators {
        if env::var("AGAVE_KNOWN_VALIDATORS").is_err() {
            env::set_var("AGAVE_KNOWN_VALIDATORS", kv.join(","));
            applied += 1;
        }
    }

    info!("Loaded config from {} (applied {} key(s))", path, applied);
}


fn emit_config_conflict_warnings(matches: &ArgMatches) {
    let args: Vec<String> = env::args().collect();
    let config_path = args
        .windows(2)
        .find(|pair| pair[0] == "--config")
        .map(|pair| pair[1].clone());

    let Some(path) = config_path else {
        return;
    };

    let Ok(config) = ValidatorConfig::load_from_file(&path) else {
        return;
    };


    if config.ledger.is_some() && matches.is_present("ledger_path") {
        warn!("Config 'ledger' overridden by CLI --ledger");
    }
    if config.identity.is_some() && matches.is_present("identity") {
        warn!("Config 'identity' overridden by CLI --identity");
    }
    if config.entrypoint.as_ref().map_or(false, |v| !v.is_empty())
        && matches.is_present("entrypoint")
    {
        warn!("Config 'entrypoint' overridden by CLI --entrypoint");
    }


    if config.log.as_ref().map_or(false, |s| !s.is_empty()) && matches.is_present("log") {
        warn!("Config 'log' overridden by CLI --log");
    }
    if config.rpc_port.is_some() && matches.is_present("rpc_port") {
        warn!("Config 'rpc_port' overridden by CLI --rpc_port");
    }
    if config
        .known_validators
        .as_ref()
        .map_or(false, |v| !v.is_empty())
        && matches.is_present("known_validators")
    {
        warn!("Config 'known_validators' overridden by CLI --known-validator");
    }
}