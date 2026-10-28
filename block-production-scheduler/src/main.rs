use {
    agave_block_production_scheduler::{Config, run},
    clap::{App, Arg},
    core::sync::atomic::AtomicBool,
    log::{debug, error, info},
    signal_hook::{
        consts::signal::{SIGINT, SIGTERM},
        flag,
    },
    std::{fs, path::Path, process, sync::Arc},
};

fn main() {
    agave_logger::setup_with_default_filter();
    let matches = App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .about("Run the Agave block production scheduler as an external process")
        .arg(
            Arg::with_name("config")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .required(true)
                .help(
                    "TOML configuration file; ipc_path is required, other settings override the \
                     embedded defaults",
                ),
        )
        .get_matches();

    let config_path = Path::new(matches.value_of_os("config").unwrap());
    let source = match fs::read_to_string(config_path) {
        Ok(source) => source,
        Err(err) => {
            error!(
                "Failed to read scheduler config {}: {err}",
                config_path.display()
            );
            process::exit(1);
        }
    };
    let config = match Config::from_toml(&source) {
        Ok(config) => config,
        Err(err) => {
            error!(
                "Failed to parse scheduler config {}: {err}",
                config_path.display()
            );
            process::exit(1);
        }
    };
    agave_logger::initialize_logging(config.log_file.clone());
    info!(
        "Loaded scheduler configuration from {}",
        config_path.display()
    );
    debug!("Scheduler configuration: {config:?}");
    let exit = Arc::new(AtomicBool::new(false));
    if let Err(err) = flag::register(SIGINT, Arc::clone(&exit)) {
        error!("Failed to register SIGINT handler: {err}");
        process::exit(1);
    }
    if let Err(err) = flag::register(SIGTERM, Arc::clone(&exit)) {
        error!("Failed to register SIGTERM handler: {err}");
        process::exit(1);
    }
    if let Err(err) = run(config, &exit) {
        error!("Scheduler failed: {err}");
        process::exit(1);
    }
}
