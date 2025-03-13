use {
    solana_gossip::{
        cluster_info::ClusterInfo, contact_info::ContactInfo, gossip_service::make_gossip_node,
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::{EncodableKey, EncodableKeypair, Signer, SignerError},
    solana_streamer::socket::SocketAddrSpace,
    std::{
        net::SocketAddr,
        sync::{atomic::AtomicBool, Arc},
    },
};

fn main() {
    let keypair = Arc::new(Keypair::new());
    let pubkey = keypair.pubkey();
    let shred_version = 42;
    let gossip_addr = SocketAddr::from(([0, 0, 0, 0], 4433));

    let exit = Arc::new(AtomicBool::new(false));

    let (gossip_svc, ip_echo, cluster_info) = make_gossip_node(
        keypair.insecure_clone(),
        None,
        exit.clone(),
        Some(&gossip_addr),
        shred_version,
        false,
        SocketAddrSpace::Global,
    );
    let mut contact_info = ContactInfo::new(pubkey, 0, 42);
    contact_info
        .set_gossip(([0, 0, 0, 0], 4433))
        .expect("Should have valid IP");

    println!("Hello, world!");
}
