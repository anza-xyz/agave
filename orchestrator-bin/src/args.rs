use {clap::Parser, clap_v4 as clap, std::path::PathBuf};

#[derive(Parser)]
#[command(name = "agave-orchestrator")]
pub(crate) struct Args {
    /// Path to the orchestrator TOML config file.
    #[arg(long)]
    pub(crate) config: PathBuf,
}
