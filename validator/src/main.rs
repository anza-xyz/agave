#![allow(clippy::arithmetic_side_effects)]
#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
use jemallocator::Jemalloc;
use {
    agave_validator::{
        cli::{app, warn_for_deprecated_arguments, DefaultArgs},
        commands,
    },
    solana_streamer::socket::SocketAddrSpace,
    std::path::PathBuf,
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

    let socket_addr_space = SocketAddrSpace::new(matches.is_present("allow_private_addr"));
    let ledger_path = PathBuf::from(matches.value_of("ledger_path").unwrap());

    match matches.subcommand() {
        ("init", _) => {
            commands::run::execute(
                &matches,
                solana_version,
                socket_addr_space,
                &ledger_path,
                commands::run::execute::Operation::Initialize,
            );
        }
        ("", _) | ("run", _) => {
            commands::run::execute(
                &matches,
                solana_version,
                socket_addr_space,
                &ledger_path,
                commands::run::execute::Operation::Run,
            );
        }
        ("authorized-voter", Some(authorized_voter_subcommand_matches)) => {
            commands::authorized_voter::execute(authorized_voter_subcommand_matches, &ledger_path);
        }
        ("plugin", Some(plugin_subcommand_matches)) => {
            commands::plugin::execute(plugin_subcommand_matches, &ledger_path);
        }
        ("contact-info", Some(subcommand_matches)) => {
            commands::contact_info::execute(subcommand_matches, &ledger_path);
        }
        ("exit", Some(subcommand_matches)) => {
            commands::exit::execute(subcommand_matches, &ledger_path);
        }
        ("monitor", _) => {
            commands::monitor::execute(&matches, &ledger_path);
        }
        ("staked-nodes-overrides", Some(subcommand_matches)) => {
            commands::staked_nodes_overrides::execute(subcommand_matches, &ledger_path);
        }
        ("set-identity", Some(subcommand_matches)) => {
            commands::set_identity::execute(subcommand_matches, &ledger_path);
        }
        ("set-log-filter", Some(subcommand_matches)) => {
            commands::set_log_filter::execute(subcommand_matches, &ledger_path);
        }
        ("wait-for-restart-window", Some(subcommand_matches)) => {
            commands::wait_for_restart_window::execute(subcommand_matches, &ledger_path);
        }
        ("repair-shred-from-peer", Some(subcommand_matches)) => {
            commands::repair_shred_from_peer::execute(subcommand_matches, &ledger_path);
        }
        ("repair-whitelist", Some(repair_whitelist_subcommand_matches)) => {
            commands::repair_whitelist::execute(repair_whitelist_subcommand_matches, &ledger_path);
        }
        ("set-public-address", Some(subcommand_matches)) => {
            commands::set_public_address::execute(subcommand_matches, &ledger_path);
        }
        _ => unreachable!(),
    };
}
