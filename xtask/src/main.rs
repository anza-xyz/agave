use clap::{Args, Parser, Subcommand};

mod commands;
mod common;

#[derive(Parser)]
#[command(name = "xtask", about = "Custom build tasks", version)]
struct Xtask {
    #[command(flatten)]
    pub global: GlobalOptions,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Hello world")]
    Hello,
    #[command(about = "Bump version")]
    BumpVersion(commands::bump_version::BumpArgs),
}

#[derive(Args, Debug)]
pub struct GlobalOptions {
    /// Enable verbose (debug) logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

fn main() {
    if let Err(e) = try_main() {
        eprintln!("{}", e);
        std::process::exit(-1);
    }
}

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    let xtask = Xtask::parse();

    if xtask.global.verbose {
        std::env::set_var("RUST_LOG", "debug");
    } else {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    match xtask.command {
        Commands::Hello => {
            let a = "=1.0.0";
            println!("{}", a.replace("1.0.0", "1.0.1"));
        }
        Commands::BumpVersion(args) => {
            commands::bump_version::run(args)?;
        }
    }
    Ok(())
}
