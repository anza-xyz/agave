#![allow(clippy::arithmetic_side_effects)]
#[macro_use]
extern crate log;

use {
    rayon::iter::*,
    solana_gossip::{
        cluster_info::ClusterInfo,
        contact_info::{ContactInfo, Protocol},
        gossip_service::GossipService,
        node::Node,
    },
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_perf::packet::PACKET_DATA_SIZE,
    solana_signer::Signer,
    solana_streamer::sendmmsg::{SendPktsError, multi_target_send},
    solana_time_utils::timestamp,
    std::{
        net::UdpSocket,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::sleep,
        time::Duration,
    },
};

fn test_node(exit: Arc<AtomicBool>) -> (Arc<ClusterInfo>, GossipService, UdpSocket) {
    let keypair = Arc::new(Keypair::new());
    let mut test_node = Node::new_localhost_with_pubkey(&keypair.pubkey());
    let cluster_info = Arc::new(ClusterInfo::new(
        test_node.info.clone(),
        keypair,
        SocketAddrSpace::Unspecified,
    ));
    let gossip_service = GossipService::new(
        &cluster_info,
        None,
        test_node.sockets.gossip,
        None,
        None,
        true, // should_check_duplicate_instance
        None,
        exit,
    );
    let _ = cluster_info.my_contact_info();
    (
        cluster_info,
        gossip_service,
        test_node.sockets.tvu.pop().unwrap(),
    )
}

/// Test that the network converges.
/// Run until every node in the network has a full ContactInfo set.
/// Check that nodes stop sending updates after all the ContactInfo has been shared.
/// tests that actually use this function are below
fn run_gossip_topo<F>(num: usize, topo: F)
where
    F: Fn(&Vec<(Arc<ClusterInfo>, GossipService, UdpSocket)>),
{
    let exit = Arc::new(AtomicBool::new(false));
    let listen: Vec<_> = (0..num).map(|_| test_node(exit.clone())).collect();
    topo(&listen);
    let mut done = false;
    for i in 0..(num * 32) {
        let total: usize = listen.iter().map(|v| v.0.gossip_peers().len()).sum();
        if (total + num) * 10 > num * num * 9 {
            done = true;
            break;
        } else {
            trace!("not converged {} {} {}", i, total + num, num * num);
        }
        sleep(Duration::from_secs(1));
    }
    exit.store(true, Ordering::Relaxed);
    for (_, dr, _) in listen {
        dr.join().unwrap();
    }
    assert!(done);
}

/// retransmit messages to a list of nodes
fn retransmit_to(
    peers: &[&ContactInfo],
    data: &[u8],
    socket: &UdpSocket,
    forwarded: bool,
    socket_addr_space: &SocketAddrSpace,
) {
    trace!("retransmit orders {}", peers.len());
    let dests: Vec<_> = if forwarded {
        peers
            .iter()
            .filter_map(|peer| peer.tvu(Protocol::UDP))
            .filter(|addr| socket_addr_space.check(addr))
            .collect()
    } else {
        peers
            .iter()
            .filter_map(|peer| peer.tvu(Protocol::UDP))
            .filter(|addr| socket_addr_space.check(addr))
            .collect()
    };
    if let Err(SendPktsError::IoError(ioerr)) = multi_target_send(socket, data, &dests) {
        error!(
            "retransmit_to multi_target_send error: {:?}, {} destinations",
            ioerr,
            dests.len(),
        );
    }
}

/// ring a -> b -> c -> d -> e -> a
#[test]
fn gossip_ring() {
    agave_logger::setup();
    run_gossip_topo(40, |listen| {
        let num = listen.len();
        for n in 0..num {
            let y = n % listen.len();
            let x = (n + 1) % listen.len();
            let yv = &listen[y].0;
            let mut d = yv.lookup_contact_info(&yv.id(), |ci| ci.clone()).unwrap();
            d.set_wallclock(timestamp());
            listen[x].0.insert_info(d);
        }
    });
}

/// ring a -> b -> c -> d -> e -> a
#[test]
#[ignore]
fn gossip_ring_large() {
    agave_logger::setup();
    run_gossip_topo(600, |listen| {
        let num = listen.len();
        for n in 0..num {
            let y = n % listen.len();
            let x = (n + 1) % listen.len();
            let yv = &listen[y].0;
            let mut d = yv.lookup_contact_info(&yv.id(), |ci| ci.clone()).unwrap();
            d.set_wallclock(timestamp());
            listen[x].0.insert_info(d);
        }
    });
}
/// star a -> (b,c,d,e)
#[test]
fn gossip_star() {
    agave_logger::setup();
    run_gossip_topo(10, |listen| {
        let num = listen.len();
        for n in 0..(num - 1) {
            let x = 0;
            let y = (n + 1) % listen.len();
            let yv = &listen[y].0;
            let mut yd = yv.lookup_contact_info(&yv.id(), |ci| ci.clone()).unwrap();
            yd.set_wallclock(timestamp());
            let xv = &listen[x].0;
            xv.insert_info(yd);
            trace!("star leader {}", xv.id());
        }
    });
}

/// rstar a <- (b,c,d,e)
#[test]
fn gossip_rstar() {
    agave_logger::setup();
    run_gossip_topo(10, |listen| {
        let num = listen.len();
        let xd = {
            let xv = &listen[0].0;
            xv.lookup_contact_info(&xv.id(), |ci| ci.clone()).unwrap()
        };
        trace!("rstar leader {}", xd.pubkey());
        for n in 0..(num - 1) {
            let y = (n + 1) % listen.len();
            let yv = &listen[y].0;
            yv.insert_info(xd.clone());
            trace!("rstar insert {} into {}", xd.pubkey(), yv.id());
        }
    });
}

#[test]
pub fn cluster_info_retransmit() {
    agave_logger::setup();
    let exit = Arc::new(AtomicBool::new(false));
    trace!("c1:");
    let (c1, dr1, tn1) = test_node(exit.clone());
    trace!("c2:");
    let (c2, dr2, tn2) = test_node(exit.clone());
    trace!("c3:");
    let (c3, dr3, tn3) = test_node(exit.clone());
    let c1_contact_info = c1.my_contact_info();

    c2.insert_info(c1_contact_info.clone());
    c3.insert_info(c1_contact_info);

    let num = 3;

    //wait to converge
    trace!("waiting to converge:");
    let mut done = false;
    for _ in 0..30 {
        done = c1.gossip_peers().len() == num - 1
            && c2.gossip_peers().len() == num - 1
            && c3.gossip_peers().len() == num - 1;
        if done {
            break;
        }
        sleep(Duration::from_secs(1));
    }
    assert!(done);
    let data = [0u8; 10];
    let peers = c1.tvu_peers(ContactInfo::clone);
    let retransmit_peers: Vec<_> = peers.iter().collect();
    retransmit_to(
        &retransmit_peers,
        &data,
        &tn1,
        false,
        &SocketAddrSpace::Unspecified,
    );
    let res: Vec<_> = [tn1, tn2, tn3]
        .into_par_iter()
        .map(|s| {
            let mut buffer = [0u8; PACKET_DATA_SIZE];
            s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let res = s.recv_from(&mut buffer);
            res.is_err() //true if failed to receive the retransmit packet
        })
        .collect();
    //true if failed receive the retransmit packet, r2, and r3 should succeed
    //r1 was the sender, so it should fail to receive the packet
    assert_eq!(res, [true, false, false]);
    exit.store(true, Ordering::Relaxed);
    dr1.join().unwrap();
    dr2.join().unwrap();
    dr3.join().unwrap();
}
