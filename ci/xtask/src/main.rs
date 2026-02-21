use {
    anyhow::Result,
    clap::{Args, Parser, Subcommand},
    log::error,
};

mod commands;
mod common;

#[derive(Parser)]
#[command(name = "xtask", about = "Build tasks", version)]
struct Xtask {
    #[command(flatten)]
    pub global: GlobalOptions,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Hello")]
    Hello,
    #[command(about = "Bump version")]
    BumpVersion(commands::bump_version::CommandArgs),
    #[command(about = "Update crate version")]
    UpdateCrate(commands::update_crate::CommandArgs),
    #[command(about = "Publish crates")]
    Publish(commands::publish::CommandArgs),
}

#[derive(Args, Debug)]
pub struct GlobalOptions {
    /// Enable verbose (debug) logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

fn main() {
    // parse the command line arguments
    let xtask = Xtask::parse();

    // set the log level
    // Safety: no threads are spawned at this point, so no parallel env updates can happen
    unsafe {
        if xtask.global.verbose {
            std::env::set_var("RUST_LOG", "debug");
        } else {
            std::env::set_var("RUST_LOG", "info");
        }
    }
    env_logger::init();

    let rt = tokio::runtime::Runtime::new().expect("must create runtime");
    if let Err(err) = rt.block_on(try_main(xtask)) {
        error!("Error: {err}");
        for (i, cause) in err.chain().skip(1).enumerate() {
            error!("  {}: {}", i.saturating_add(1), cause);
        }
        std::process::exit(1);
    }
}

async fn try_main(xtask: Xtask) -> Result<()> {
    // run the command
    match xtask.command {
        Commands::Hello => commands::hello::run()?,
        Commands::BumpVersion(args) => {
            commands::bump_version::run(args)?;
        }
        Commands::UpdateCrate(args) => {
            commands::update_crate::run(args)?;
        }
        Commands::Publish(args) => {
            commands::publish::run(args)?;
        }
    }

    Ok(())
}
