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

    let config_file = match load_config(&matches) {
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
            &config_file,
        )
        .inspect_err(|err| error!("Failed to initialize validator: {err}"))
        .map_err(commands::Error::Dynamic),
        ("", _) | ("run", _) => commands::run::execute(
            &matches,
            solana_version,
            commands::run::execute::Operation::Run,
            &config_file,
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        clap::{App, Arg},
        std::{fs, io::Write},
        tempfile::tempdir,
    };

    fn make_app() -> App<'static, 'static> {
        App::new("test").arg(
            Arg::with_name("config")
                .long("config")
                .takes_value(true)
                .multiple(true),
        )
    }

    #[test]
    fn load_config_without_flag_returns_default() {
        let app = make_app();
        let matches = app.get_matches_from(["test"]);

        let cfg = load_config(&matches).expect("load_config failed");

        // Default ConfigFile behavior
        assert!(cfg.cpu_reservations.is_empty());
        assert!(cfg.net.xdp.interface.is_none());
        assert_eq!(cfg.net.xdp.zero_copy, None);
    }

    #[test]
    fn load_config_reads_single_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("agave.toml");

        let toml_str = r#"
            cpu-reservations = [
              { cores = [1, 2, 3], scope = "xdp" },
              { cores = [10],      scope = "poh" },
            ]

            [net.xdp]
            interface = "eno12399np0"
            zero_copy = true
        "#;

        {
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(toml_str.as_bytes()).unwrap();
        }

        let app = make_app();
        let matches = app.get_matches_from(["test", "--config", path.to_str().unwrap()]);

        let cfg = load_config(&matches).expect("load_config failed");

        assert_eq!(cfg.net.xdp.interface.as_deref(), Some("eno12399np0"));
        assert_eq!(cfg.net.xdp.zero_copy, Some(true));

        let xdp = cfg.xdp_cpus().expect("expected xdp cpus");
        assert_eq!(xdp, vec![1, 2, 3]);

        assert_eq!(cfg.poh_cpu(), Some(10));
    }

    #[test]
    fn load_config_reads_directory_of_files() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let file1 = dir_path.join("xdp.toml");
        let file2 = dir_path.join("cpus.toml");

        fs::write(
            &file1,
            r#"
                [net.xdp]
                interface = "eth0"
                zero_copy = false
            "#,
        )
        .unwrap();

        fs::write(
            &file2,
            r#"
                cpu-reservations = [
                  { cores = [4, 5], scope = "xdp" },
                  { cores = [11],   scope = "poh" },
                ]
            "#,
        )
        .unwrap();

        let app = make_app();
        let matches = app.get_matches_from(["test", "--config", dir_path.to_str().unwrap()]);

        let cfg = load_config(&matches).expect("load_config failed");

        assert_eq!(cfg.net.xdp.interface.as_deref(), Some("eth0"));
        assert_eq!(cfg.net.xdp.zero_copy, Some(false));

        let xdp = cfg.xdp_cpus().expect("expected xdp cpus");
        assert_eq!(xdp, vec![4, 5]);

        assert_eq!(cfg.poh_cpu(), Some(11));
    }

    #[test]
    fn load_config_multiple_config_args_merges_all() {
        // Two separate files, passed as two --config args.
        let dir = tempdir().unwrap();
        let path1 = dir.path().join("a.toml");
        let path2 = dir.path().join("b.toml");

        fs::write(
            &path1,
            r#"
                [net.xdp]
                interface = "eth1"
            "#,
        )
        .unwrap();

        fs::write(
            &path2,
            r#"
                cpu-reservations = [
                  { cores = [7, 8], scope = "xdp" },
                ]
            "#,
        )
        .unwrap();

        let app = make_app();
        let matches = app.get_matches_from([
            "test",
            "--config",
            path1.to_str().unwrap(),
            "--config",
            path2.to_str().unwrap(),
        ]);

        let cfg = load_config(&matches).expect("load_config failed");

        assert_eq!(cfg.net.xdp.interface.as_deref(), Some("eth1"));

        let xdp = cfg.xdp_cpus().expect("expected xdp cpus");
        assert_eq!(xdp, vec![7, 8]);
    }

    #[test]
    fn no_cpu_reservations_yields_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("agave.toml");

        fs::write(
            &path,
            r#"
                [net.xdp]
                interface = "eth0"
                zero_copy = false
            "#,
        )
        .unwrap();

        let app = make_app();
        let matches = app.get_matches_from(["test", "--config", path.to_str().unwrap()]);

        let cfg = load_config(&matches).expect("load_config failed");

        assert!(cfg.cpu_reservations.is_empty());
        assert_eq!(cfg.xdp_cpus(), None);
        assert_eq!(cfg.poh_cpu(), None);
        assert_eq!(cfg.net.xdp.interface.as_deref(), Some("eth0"));
        assert_eq!(cfg.net.xdp.zero_copy, Some(false));
    }

    #[test]
    fn poh_without_cores_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("agave.toml");

        fs::write(
            &path,
            r#"
                cpu-reservations = [
                  { scope = "poh" },
                ]
            "#,
        )
        .unwrap();

        let app = make_app();
        let matches = app.get_matches_from(["test", "--config", path.to_str().unwrap()]);

        let cfg = load_config(&matches).expect("load_config failed");
        assert_eq!(cfg.poh_cpu(), None);
    }

    #[test]
    #[should_panic(expected = "Cannot have more than 1 poh cpu")]
    fn poh_multiple_cores_panics() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("agave.toml");

        fs::write(
            &path,
            r#"
                cpu-reservations = [
                  { cores = [10, 11], scope = "poh" },
                ]
            "#,
        )
        .unwrap();

        let app = make_app();
        let matches = app.get_matches_from(["test", "--config", path.to_str().unwrap()]);

        let cfg = load_config(&matches).expect("load_config failed");
        let _ = cfg.poh_cpu();
    }
}
