use {
    agave_block_production_scheduler::{Config, run},
    clap::{App, Arg},
    core::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    },
    log::{debug, error, info},
    signal_hook::{
        consts::signal::{SIGINT, SIGTERM, SIGUSR1},
        flag,
    },
    std::{
        fs, io,
        path::{Path, PathBuf},
        process,
        sync::Arc,
        thread,
    },
};

const LOG_ROTATION_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

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
    let exit = Arc::new(AtomicBool::new(false));
    if let Some(log_file) = config.log_file.clone()
        && let Err(err) = start_log_rotation(log_file, Arc::clone(&exit))
    {
        error!("Failed to start log rotation: {err}");
        process::exit(1);
    }
    info!(
        "Loaded scheduler configuration from {}",
        config_path.display()
    );
    debug!("Scheduler configuration: {config:?}");
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

fn start_log_rotation(log_file: PathBuf, exit: Arc<AtomicBool>) -> io::Result<()> {
    let rotate = Arc::new(AtomicBool::new(false));
    flag::register(SIGUSR1, Arc::clone(&rotate))?;
    thread::Builder::new()
        .name("scheduler-logrotate".into())
        .spawn(move || {
            while !exit.load(Ordering::Relaxed) {
                if rotate.swap(false, Ordering::Relaxed) {
                    info!("Received SIGUSR1, reopening {}", log_file.display());
                    agave_logger::redirect_stderr(&log_file);
                }
                thread::sleep(LOG_ROTATION_POLL_INTERVAL);
            }
        })?;
    Ok(())
}
