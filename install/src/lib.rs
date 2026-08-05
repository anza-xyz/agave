#![allow(clippy::arithmetic_side_effects)]
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand, crate_description, crate_name};

mod build_env;
mod command;
mod config;
mod defaults;
mod stop_process;

pub fn is_semver(semver: &str) -> Result<(), String> {
    match semver::Version::parse(semver) {
        Ok(_) => Ok(()),
        Err(err) => Err(format!("{err:?}")),
    }
}

pub fn is_release_channel(channel: &str) -> Result<(), String> {
    match channel {
        "edge" | "beta" | "stable" => Ok(()),
        _ => Err(format!("Invalid release channel {channel}")),
    }
}

pub fn is_explicit_release(string: String) -> Result<(), String> {
    if string.starts_with('v') && is_semver(string.split_at(1).1).is_ok() {
        return Ok(());
    }
    is_semver(&string).or_else(|_| is_release_channel(&string))
}

pub fn explicit_release_of(matches: &ArgMatches<'_>, name: &str) -> config::ExplicitRelease {
    let explicit_release = matches.value_of(name).unwrap().to_string();
    if explicit_release.starts_with('v') && is_semver(explicit_release.split_at(1).1).is_ok() {
        config::ExplicitRelease::Semver(explicit_release.split_at(1).1.to_string())
    } else if is_semver(&explicit_release).is_ok() {
        config::ExplicitRelease::Semver(explicit_release.to_owned())
    } else {
        config::ExplicitRelease::Channel(explicit_release.to_owned())
    }
}

fn handle_init(matches: &ArgMatches<'_>, config_file: &str) -> Result<(), String> {
    let data_dir = matches.value_of("data_dir").unwrap();
    let no_modify_path = matches.is_present("no_modify_path");
    let explicit_release = explicit_release_of(matches, "explicit_release");

    command::init(config_file, data_dir, no_modify_path, explicit_release)
}

pub fn main() -> Result<(), String> {
    agave_logger::setup();

    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .arg({
            let arg = Arg::with_name("config_file")
                .short("c")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            match *defaults::CONFIG_FILE {
                Some(ref config_file) => arg.default_value(config_file),
                None => arg.required(true),
            }
        })
        .subcommand(
            SubCommand::with_name("init")
                .about("Initializes a new installation")
                .setting(AppSettings::DisableVersion)
                .arg({
                    let arg = Arg::with_name("data_dir")
                        .short("d")
                        .long("data-dir")
                        .value_name("PATH")
                        .takes_value(true)
                        .required(true)
                        .help("Directory to store install data");
                    match *defaults::DATA_DIR {
                        Some(ref data_dir) => arg.default_value(data_dir),
                        None => arg,
                    }
                })
                .arg(
                    Arg::with_name("no_modify_path")
                        .long("no-modify-path")
                        .help("Don't configure the PATH environment variable"),
                )
                .arg(
                    Arg::with_name("explicit_release")
                        .value_name("release")
                        .index(1)
                        .validator(is_explicit_release)
                        .required(true)
                        .help("The release version or channel to install"),
                ),
        )
        .subcommand(
            SubCommand::with_name("info")
                .about("Displays information about the current installation")
                .setting(AppSettings::DisableVersion)
                .arg(
                    Arg::with_name("local_info_only")
                        .short("l")
                        .long("local")
                        .help("Only display local information, don't check for updates"),
                )
                .arg(
                    Arg::with_name("eval")
                        .long("eval")
                        .help("Display information in a format that can be used with `eval`"),
                ),
        )
        .subcommand(
            SubCommand::with_name("gc")
                .about("Delete older releases from the install cache to reclaim disk space")
                .setting(AppSettings::DisableVersion),
        )
        .subcommand(
            SubCommand::with_name("update")
                .about("Checks for an update, and if available downloads and applies it")
                .setting(AppSettings::DisableVersion),
        )
        .subcommand(
            SubCommand::with_name("run")
                .about("Runs a program while periodically checking and applying software updates")
                .after_help("The program will be restarted upon a successful software update")
                .setting(AppSettings::DisableVersion)
                .arg(
                    Arg::with_name("program_name")
                        .index(1)
                        .required(true)
                        .help("program to run"),
                )
                .arg(
                    Arg::with_name("program_arguments")
                        .index(2)
                        .multiple(true)
                        .help("Arguments to supply to the program"),
                ),
        )
        .subcommand(SubCommand::with_name("list").about("List installed versions of solana cli"))
        .get_matches();

    let config_file = matches.value_of("config_file").unwrap();

    match matches.subcommand() {
        ("init", Some(matches)) => handle_init(matches, config_file),
        ("info", Some(matches)) => {
            let local_info_only = matches.is_present("local_info_only");
            let eval = matches.is_present("eval");
            command::info(config_file, local_info_only, eval).map(|_| ())
        }
        ("gc", Some(_matches)) => command::gc(config_file),
        ("update", Some(_matches)) => command::update(config_file, false).map(|_| ()),
        ("run", Some(matches)) => {
            let program_name = matches.value_of("program_name").unwrap();
            let program_arguments = matches
                .values_of("program_arguments")
                .map(Iterator::collect)
                .unwrap_or_else(Vec::new);

            command::run(config_file, program_name, program_arguments)
        }
        ("list", Some(_matches)) => command::list(config_file),
        _ => unreachable!(),
    }
}

pub fn main_init() -> Result<(), String> {
    agave_logger::setup();

    let matches = App::new("agave-install-init")
        .about("Initializes a new installation")
        .version(solana_version::version!())
        .arg({
            let arg = Arg::with_name("config_file")
                .short("c")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .help("Configuration file to use");
            match *defaults::CONFIG_FILE {
                Some(ref config_file) => arg.default_value(config_file),
                None => arg.required(true),
            }
        })
        .arg({
            let arg = Arg::with_name("data_dir")
                .short("d")
                .long("data-dir")
                .value_name("PATH")
                .takes_value(true)
                .required(true)
                .help("Directory to store install data");
            match *defaults::DATA_DIR {
                Some(ref data_dir) => arg.default_value(data_dir),
                None => arg,
            }
        })
        .arg(
            Arg::with_name("no_modify_path")
                .long("no-modify-path")
                .help("Don't configure the PATH environment variable"),
        )
        .arg(
            Arg::with_name("explicit_release")
                .value_name("release")
                .index(1)
                .validator(is_explicit_release)
                .required(true)
                .help("The release version or channel to install"),
        )
        .get_matches();

    let config_file = matches.value_of("config_file").unwrap();
    handle_init(&matches, config_file)
}
