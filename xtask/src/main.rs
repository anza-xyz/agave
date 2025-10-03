use {
    anyhow::Result,
    clap::{Args, Parser, Subcommand},
    log::{error, info},
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
    BumpVersion(commands::bump_version::BumpArgs),
    #[command(about = "Generate pipeline")]
    GeneratePipeline(commands::generate_pipeline::GeneratePipelineArgs),
}

#[derive(Args, Debug)]
pub struct GlobalOptions {
    /// Enable verbose (debug) logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[tokio::main]
async fn main() {
    if let Err(err) = try_main().await {
        error!("Error: {err}");
        for (i, cause) in err.chain().skip(1).enumerate() {
            error!("  {}: {}", i.saturating_add(1), cause);
        }
        std::process::exit(1);
    }
}

async fn try_main() -> Result<()> {
    // parse the command line arguments
    let xtask = Xtask::parse();

    // set the log level
    if xtask.global.verbose {
        std::env::set_var("RUST_LOG", "debug");
    } else {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    // run the command
    match xtask.command {
        Commands::Hello => {
            info!("Hello!");
        }
        Commands::BumpVersion(args) => {
            commands::bump_version::run(args)?;
        }
        Commands::GeneratePipeline(args) => {
            commands::generate_pipeline::run(args).await?;
        }
    }
    Ok(())
}
