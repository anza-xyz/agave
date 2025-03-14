use {
    log::error,
    signal_hook::{consts::SIGINT, iterator::Signals},
    solana_gossip::{
        contact_info::ContactInfo,
        crds::{Crds, GossipRoute},
        crds_data::CrdsData,
        crds_gossip::CrdsGossip,
        crds_value::CrdsValue,
        gossip_service::make_gossip_node,
    },
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_streamer::socket::SocketAddrSpace,
    solana_time_utils::timestamp,
    std::{
        net::SocketAddr,
        sync::{atomic::AtomicBool, Arc},
        thread::{self},
        time::Duration,
    },
};
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
    let exit = Arc::new(AtomicBool::new(false));
    solana_logger::setup_with("info,solana_metrics=error");
    let mut signals = Signals::new([SIGINT])?;

    thread::spawn({
        let exit = exit.clone();
        move || {
            if let Some(sig) = signals.forever().next() {
                println!("Received signal {:?}", sig);
                exit.store(true, std::sync::atomic::Ordering::Relaxed);
                // Wait for workers to ack that they are exiting
                thread::sleep(Duration::from_secs(1));

                if exit.load(std::sync::atomic::Ordering::Relaxed) {
                    println!("Timed out waiting for capture to stop, aborting!");
                    std::process::exit(1);
                }
            }
        }
    });
    let keypair = Arc::new(Keypair::new());
    let pubkey = keypair.pubkey();
    let shred_version = 42;
    let gossip_addr = SocketAddr::from(([147, 28, 133, 67], 4433));

    let (gossip_svc, _ip_echo, cluster_info) = make_gossip_node(
        keypair.insecure_clone(),
        None,
        exit.clone(),
        Some(&gossip_addr),
        shred_version,
        false,
        SocketAddrSpace::Global,
    );
    thread::sleep(Duration::from_secs(5));
    let mut contact_info = ContactInfo::new(pubkey, timestamp(), 42);
    contact_info
        .set_gossip(SocketAddr::from(([1, 2, 3, 4], 666)))
        .expect("Should have valid IP");
    let peer1 = Keypair::new();
    //insert_contact_info(&cluster_info.gossip, &peer1, contact_info);
    gossip_svc.join().unwrap();
    println!("Hello, world!");
    Ok(())
}
