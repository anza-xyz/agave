use {
    crate::SHRED_VERSION,
    clap::Subcommand,
    log::error,
    serde::{Deserialize, Serialize},
    serde_json::json,
    solana_gossip::{
        cluster_info::ClusterInfo,
        contact_info::ContactInfo,
        crds::{Cursor, GossipRoute},
        crds_data::CrdsData,
        crds_gossip::CrdsGossip,
        crds_value::CrdsValue,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_time_utils::timestamp,
    solana_transaction::Transaction,
    std::{net::SocketAddr, ops::ControlFlow},
};
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub(crate) enum Command {
    Exit,
    /// Send a random Vote CRDS
    SendVote,
    /// Send a ping and get pong back (does not actually work yet)
    SendPing {
        #[arg(long)]
        target: Pubkey,
        #[arg(long)]
        target_addr: Option<SocketAddr>,
    },
    /// Return list of peers currently in CRDS
    Peers,
    /// Returns epoch slots inserted since given slot
    EpochSlots {
        #[arg(short, long)]
        slot: u64,
    },
    /// Returns votes inserted since given slot
    Votes {
        #[arg(short, long)]
        slot: u64,
    },
    /// Insert a ContactInfo for provided peer
    InsertContactInfo {
        /// Keypair for the new peer to be inserted. If not provided,
        /// a random identity will be generated
        #[arg(short, long)]
        keypair: Option<String>,
        /// Address of the new peer
        #[arg(short, long)]
        address: SocketAddr,
    },
}

fn insert_contact_info(gossip: &CrdsGossip, keypair: Keypair, address: SocketAddr) {
    let pubkey = keypair.pubkey();
    let mut contact_info = ContactInfo::new(pubkey, timestamp(), SHRED_VERSION);
    contact_info
        .set_gossip(address)
        .expect("Should have valid gossip address");
    let entry = CrdsValue::new(CrdsData::ContactInfo(contact_info), &keypair);

    if let Err(err) = {
        let mut gossip_crds = gossip.crds.write().unwrap();
        gossip_crds.insert(entry, timestamp(), GossipRoute::LocalMessage)
    } {
        error!("ClusterInfo.insert_info: {err:?}");
    }
}

pub(crate) fn execute_command(
    cluster_info: &ClusterInfo,
    my_keypair: &Keypair,
    command: Command,
) -> anyhow::Result<ControlFlow<()>> {
    match command {
        Command::Exit => return Ok(ControlFlow::Break(())),
        Command::Peers => {
            let peers = cluster_info.all_peers();
            println!("{}", json!({ "command_ok":true , "peers": &peers}));
        }
        Command::EpochSlots { slot } => {
            let mut cursor = Cursor::new(slot);
            let epoch_slots = cluster_info.get_epoch_slots(&mut cursor);
            println!("{}", json!({ "command_ok":true , "slots":&epoch_slots}));
        }
        Command::Votes { slot } => {
            let mut cursor = Cursor::new(slot);
            let (labels, votes) = cluster_info.get_votes_with_labels(&mut cursor);
            let senders = labels.into_iter().map(|e| e.pubkey());
            let data: Vec<_> = senders.zip(votes.into_iter()).collect();
            println!("{}", json!({ "command_ok":true, "votes": &data }));
        }
        Command::SendVote => {
            let mut vote = Transaction::default();
            vote.sign(&[&my_keypair], Hash::new_unique());

            cluster_info.push_vote(&[42], vote.clone());
            println!("{}", json!({ "command_ok":true , "vote":vote}));
        }
        Command::InsertContactInfo { keypair, address } => {
            let keypair = keypair
                .map(|v| Keypair::from_base58_string(&v))
                .unwrap_or(Keypair::new());
            insert_contact_info(&cluster_info.gossip, keypair, address);
            println!("{}", json!({ "command_ok":true }));
        }
        Command::SendPing {
            target,
            target_addr,
        } => {
            let Some(target_addr) = target_addr.or_else(|| {
                cluster_info
                    .lookup_contact_info(&target, |v| v.gossip())
                    .flatten()
            }) else {
                println!(
                    "{}",
                    json!({ "command_ok":false,"status":"no address found"  })
                );
                return Ok(ControlFlow::Continue(()));
            };
            let (state, maybe_pkt) = cluster_info.check_ping(target, target_addr);
            let ping_sent = if let Some(pkt) = maybe_pkt {
                let sock = std::net::UdpSocket::bind("0.0.0.0:9999")?;
                sock.send_to(pkt.data(..).unwrap(), target_addr)?;
                true
            } else {
                false
            };
            println!(
                "{}",
                json!({ "command_ok": true , "ping_cache_status":state, "ping_sent":ping_sent})
            );
        }
    }
    Ok(ControlFlow::Continue(()))
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
