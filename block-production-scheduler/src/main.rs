use {
    agave_block_production_scheduler::{Config, run},
    clap::{App, Arg},
    core::sync::atomic::AtomicBool,
    signal_hook::{
        consts::signal::{SIGINT, SIGTERM},
        flag,
    },
    std::{fs, process, sync::Arc},
};

fn main() {
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

    let Ok(source) = fs::read_to_string(matches.value_of_os("config").unwrap()) else {
        process::exit(1);
    };
    let Ok(config) = Config::from_toml(&source) else {
        process::exit(1);
    };
    let exit = Arc::new(AtomicBool::new(false));
    if flag::register(SIGINT, Arc::clone(&exit)).is_err() {
        process::exit(1);
    }
    if flag::register(SIGTERM, Arc::clone(&exit)).is_err() {
        process::exit(1);
    }
    if run(config, &exit).is_err() {
        process::exit(1);
    }
}
