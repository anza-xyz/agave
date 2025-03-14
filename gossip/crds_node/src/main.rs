use {
    anyhow::anyhow,
    clap::{command, Parser, Subcommand},
    log::error,
    signal_hook::{consts::SIGINT, iterator::Signals},
    solana_gossip::{
        contact_info::ContactInfo,
        crds::{Crds, Cursor, GossipRoute},
        crds_data::CrdsData,
        crds_gossip::CrdsGossip,
        crds_value::CrdsValue,
        gossip_service::make_gossip_node,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_streamer::socket::SocketAddrSpace,
    solana_time_utils::timestamp,
    solana_transaction::Transaction,
    std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self},
        time::Duration,
    },
};

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

    /// Consume and produce JSON instead of human-readable data
    #[arg(short, long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliCommand {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Exit,
    SendVote,
    SendPing {
        #[arg(short, long)]
        target: Pubkey,
    },
    Peers,
    InsertContactInfo,
}

fn insert_contact_info(gossip: &CrdsGossip, from: &Keypair, node: ContactInfo) {
    let entry = CrdsValue::new(CrdsData::ContactInfo(node), &from);

    if let Err(err) = {
        let mut gossip_crds = gossip.crds.write().unwrap();
        gossip_crds.insert(entry, timestamp(), GossipRoute::LocalMessage)
    } {
        error!("ClusterInfo.insert_info: {err:?}");
    }
}

fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();
    let exit = Arc::new(AtomicBool::new(false));

    solana_logger::setup_with("info,solana_metrics=error");

    let keypair = Arc::new(
        args.keypair
            .map(|v| Keypair::from_base58_string(&v))
            .unwrap_or(Keypair::new()),
    );
    let pubkey = keypair.pubkey();
    let shred_version = 42;

    let (gossip_svc, _ip_echo, cluster_info) = make_gossip_node(
        keypair.insecure_clone(),
        args.entrypoint.as_ref(),
        exit.clone(),
        Some(&args.bind_address),
        shred_version,
        false,
        SocketAddrSpace::Unspecified,
    );
    println!("{{start_node: \"{pubkey}\"}}");

    /*let rx_thread = thread::spawn({
        let cluster_info = cluster_info.clone();
        let exit = exit.clone();
        move || {
            let mut cursor = Cursor::default();

            while !exit.load(Ordering::Relaxed) {
                let votes = cluster_info.get_votes(&mut cursor);
                println!("{:?}", votes);
                thread::sleep(Duration::from_secs(10));
            }
        }
    });*/

    let mut rl = rustyline::DefaultEditor::new()?;
    while !exit.load(Ordering::Relaxed) {
        let readline = rl.readline(">> ");
        let input_line = match readline {
            Ok(line) => line,
            Err(_) => break,
        };

        let cmd = match CliCommand::try_parse_from(
            std::iter::once("tool").chain(input_line.split(" ").map(|e| e.trim())),
        ) {
            Ok(cmd) => cmd,
            Err(e) => {
                println!("Invalid input provided, {e}");
                continue;
            }
        };
        println!("Executing {:?}", &cmd);
        match cmd.command {
            Commands::Exit => break,
            Commands::Peers => {
                let peers = cluster_info.all_peers();
                println!("{:?}", peers);
            }
            Commands::SendVote => {
                let mut vote = Transaction::default();
                vote.sign(&[&keypair], Hash::new_unique());
                cluster_info.push_vote(&[42], vote);
            }
            Commands::InsertContactInfo => {
                let peer1 = Keypair::new();
                let pubkey1 = peer1.pubkey();
                let mut contact_info = ContactInfo::new(pubkey1, timestamp(), 42);
                contact_info
                    .set_gossip(SocketAddr::from(([1, 2, 3, 4], 666)))
                    .expect("Should have valid IP");
                insert_contact_info(&cluster_info.gossip, &peer1, contact_info);
            }
            _ => {}
        }
    }
    exit.store(true, Ordering::Relaxed);
    println!("{{terminate_node: \"{pubkey}\"}}");
    //rx_thread.join().map_err(|_e| anyhow!("cannot join"))?;
    gossip_svc.join().map_err(|_e| anyhow!("cannot join"))?;
    Ok(())
}

/*use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::Instruction;
#[derive(BorshSerialize, BorshDeserialize)]
enum BankInstruction {
    Initialize,
    Deposit { lamports: u64 },
    Withdraw { lamports: u64 },
}
fn fake_transaction(keypair: &Keypair) -> Transaction {
    let bank_instruction = BankInstruction::Initialize;

    let instruction = Instruction::new_with_borsh(keypair.pubkey(), &bank_instruction, vec![]);

    let message = Message::new(&[instruction], Some(&keypair.pubkey()));

    let mut tx = Transaction::new_unsigned(message);
    let blockhash = Hash;
    tx.sign(&[keypair], blockhash);

    tx
}
*/
