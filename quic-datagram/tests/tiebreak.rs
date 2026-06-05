#[path = "common.rs"]
mod common;

use {
    bytes::Bytes,
    common::{TestNode, make_runtime, spawn_node},
    solana_pubkey::Pubkey,
    solana_quic_datagram::{allowlist::AllowAll, endpoint::Datagram},
    std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration},
};

#[test]
/// Lex-pubkey tiebreaker - verifies the simultaneous-dial race resolves to
/// a single connection per peer in each side's table.
///
/// Setup: N=32 nodes all mutually-admitted, every node sends to every other
/// at the same time.
///
/// Assertion: after warm-up settles, a follow-up "burst" of one unique
/// payload per (sender, receiver) pair must arrive exactly once at every
/// receiver - no duplicates from dual flows.
fn tiebreak_no_duplicates_after_settle() {
    const N: usize = 32;
    let rt = make_runtime();
    let nodes: Vec<TestNode> = (0..N)
        .map(|_| spawn_node(&rt, Arc::new(AllowAll)))
        .collect();
    let pubkeys: Vec<Pubkey> = nodes.iter().map(|n| n.pubkey()).collect();
    let addrs: Vec<SocketAddr> = nodes.iter().map(|n| n.addr).collect();

    // Warm-up: every node concurrently sends one payload to every other -
    // provokes the simultaneous-dial race for every pair.
    rt.block_on(async {
        let mut tasks = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            let egress = n.endpoint.egress.clone();
            let targets: Vec<Datagram> = (0..N)
                .filter(|&j| j != i)
                .map(|j| Datagram {
                    peer_pubkey: pubkeys[j],
                    peer_address: addrs[j],
                    message: Bytes::from_static(b"warmup"),
                })
                .collect();
            tasks.push(tokio::spawn(async move {
                for t in targets {
                    let _ = egress.send(t).await;
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });

    // Allow the race to resolve: WRONG_DIRECTION close needs ~1 RTT
    // (localhost: microseconds) plus task scheduling. Generous slack.
    std::thread::sleep(Duration::from_secs(2));

    // Drain warm-up ingress so it can't pollute the burst-phase counts.
    for n in &nodes {
        while n.ingress_rx.try_recv().is_ok() {}
    }

    // Burst: one unique payload per ordered pair. With clean dedup each
    // receiver sees each sender's payload exactly once.
    rt.block_on(async {
        let mut tasks = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            let egress = n.endpoint.egress.clone();
            let targets: Vec<Datagram> = (0..N)
                .filter(|&j| j != i)
                .map(|j| {
                    let payload = Bytes::copy_from_slice(format!("burst-{i}-{j}").as_bytes());
                    Datagram {
                        peer_pubkey: pubkeys[j],
                        peer_address: addrs[j],
                        message: payload,
                    }
                })
                .collect();
            tasks.push(tokio::spawn(async move {
                for t in targets {
                    let _ = egress.send(t).await;
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });

    std::thread::sleep(Duration::from_secs(1));

    let mut duplicates = Vec::new();
    let mut losses = Vec::new();
    for (j, n) in nodes.iter().enumerate() {
        let mut counts: HashMap<String, usize> = HashMap::new();
        while let Ok(d) = n.ingress_rx.try_recv() {
            let s = String::from_utf8_lossy(&d.message).into_owned();
            *counts.entry(s).or_insert(0) += 1;
        }
        for i in 0..N {
            if i == j {
                continue;
            }
            let key = format!("burst-{i}-{j}");
            let c = counts.get(&key).copied().unwrap_or(0);
            match c {
                1 => {}
                0 => losses.push(format!("node {j}: lost '{key}'")),
                _ => duplicates.push(format!("node {j}: got {c} copies of '{key}'")),
            }
        }
    }

    let total_pairs = N * (N - 1);
    assert!(
        duplicates.is_empty(),
        "{}/{} pairs duplicated (lex tiebreaker failed):\n{}",
        duplicates.len(),
        total_pairs,
        duplicates
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        losses.is_empty(),
        "{}/{} pairs lost their burst datagram (warm-up did not establish a connection):\n{}",
        losses.len(),
        total_pairs,
        losses
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
