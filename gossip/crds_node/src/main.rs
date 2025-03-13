use {
    crate::commands::{execute_command, Command},
    anyhow::anyhow,
    clap::Parser,
    serde_json::json,
    solana_gossip::gossip_service::make_gossip_node,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_streamer::socket::SocketAddrSpace,
    std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    },
};

pub(crate) const SHRED_VERSION: u16 = 42;
mod commands;
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Gossip address to bind to
    #[arg(short, long, default_value = "127.0.0.1:8001")]
    bind_address: SocketAddr,
    /// Entrypoint for the cluster.
    /// If not set, this node will become an entrypoint.
    #[arg(short, long)]
    entrypoint: Option<SocketAddr>,
    /// Keypair for the node identity. If not provided, a random keypair will be made
    #[arg(short, long)]
    keypair: Option<String>,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliCommand {
    #[command(subcommand)]
    command: Command,
}

fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();
    let exit = Arc::new(AtomicBool::new(false));

    //solana_logger::setup_with("info,solana_metrics=error");
    solana_logger::setup_with("error");

    let keypair = Arc::new(
        args.keypair
            .map(|v| Keypair::from_base58_string(&v))
            .unwrap_or(Keypair::new()),
    );
    let pubkey = keypair.pubkey();

    let (gossip_svc, _ip_echo, cluster_info) = make_gossip_node(
        keypair.insecure_clone(),
        args.entrypoint.as_ref(),
        exit.clone(),
        Some(&args.bind_address),
        SHRED_VERSION,
        false,
        SocketAddrSpace::Unspecified,
    );
    println!("{}", json!({ "start_node": pubkey }));

    let mut rl = rustyline::DefaultEditor::new()?;
    loop {
        let readline = rl.readline(">> ");
        let input_line = match readline {
            Ok(line) => line,
            Err(_) => break,
        };
        rl.add_history_entry(&input_line)?;

        let command = if input_line.starts_with("{") {
            match serde_json::from_str::<Command>(&input_line) {
                Ok(cmd) => cmd,
                Err(e) => {
                    println!("{}", json!({"command_parse_error":e.to_string()}));
                    continue;
                }
            }
        } else {
            match CliCommand::try_parse_from(
                std::iter::once("tool").chain(input_line.split(" ").map(|e| e.trim())),
            ) {
                Ok(cmd) => {
                    println!("{}", serde_json::to_string(&cmd.command)?);
                    cmd.command
                }
                Err(e) => {
                    println!("Invalid input provided, {e}");
                    continue;
                }
            }
        };
        if execute_command(&cluster_info, &keypair, command)?.is_break() {
            break;
        }
    }
    exit.store(true, Ordering::Relaxed);
    println!("{}", json!({ "terminate_node": pubkey }));
    gossip_svc.join().map_err(|_e| anyhow!("cannot join"))?;
    Ok(())
}
