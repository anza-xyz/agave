#![allow(clippy::arithmetic_side_effects)]

use {
    clap::{crate_description, crate_name, value_t_or_exit, Arg, Command},
    crossbeam_channel::unbounded,
    solana_net_utils::{bind_to_unspecified, SocketConfig},
    solana_streamer::{
        packet::{Packet, PacketBatch, PacketBatchRecycler, PACKET_DATA_SIZE},
        sendmmsg::batch_send,
        streamer::{receiver, PacketBatchReceiver, StreamerReceiveStats},
    },
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        thread::{sleep, spawn, JoinHandle, Result},
        time::{Duration, SystemTime},
    },
};

fn producer(addr: &SocketAddr, exit: Arc<AtomicBool>) -> JoinHandle<usize> {
    let send = bind_to_unspecified().unwrap();

    let batch_size = 10;
    let mut packet_batch = PacketBatch::with_capacity(batch_size);
    packet_batch.resize(batch_size, Packet::default());
    for w in packet_batch.iter_mut() {
        w.meta_mut().size = PACKET_DATA_SIZE;
        w.meta_mut().set_socket_addr(addr);
    }

    spawn(move || {
        let mut num_packets_sent = 0;
        loop {
            if exit.load(Ordering::Relaxed) {
                break;
            }

            let packets_and_addrs = packet_batch.iter().map(|packet| {
                let addr = packet.meta().socket_addr();
                let data = packet.data(..).unwrap();
                (data, addr)
            });

            batch_send(&send, packets_and_addrs).unwrap();
            num_packets_sent += batch_size;
        }
        num_packets_sent
    })
}

fn sink(exit: Arc<AtomicBool>, rvs: Arc<AtomicUsize>, r: PacketBatchReceiver) -> JoinHandle<()> {
    spawn(move || loop {
        if exit.load(Ordering::Relaxed) {
            return;
        }
        let timer = Duration::new(1, 0);
        if let Ok(packet_batch) = r.recv_timeout(timer) {
            rvs.fetch_add(packet_batch.len(), Ordering::Relaxed);
        }
    })
}

fn main() -> Result<()> {
    let matches = Command::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::new("num-recv-sockets")
                .long("num-recv-sockets")
                .value_name("NUM")
                .takes_value(true)
                .default_value("1")
                .help("Use NUM receive sockets"),
        )
        .arg(
            Arg::new("num-producers")
                .long("num-producers")
                .value_name("NUM")
                .takes_value(true)
                .help("Use this many producer threads."),
        )
        .arg(
            Arg::new("duration")
                .long("duration")
                .value_name("NUM")
                .takes_value(true)
                .default_value("5")
                .help("Run for this many seconds"),
        )
        .get_matches();

    let num_sockets = value_t_or_exit!(matches, "num-recv-sockets", usize);
    let num_producers: u64 = matches.value_of_t("num-producers").unwrap_or(4);
    let duration_secs = value_t_or_exit!(matches, "duration", u64);
    let duration = Duration::new(duration_secs, 0);

    let port = 0;
    let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let (_port, read_sockets) = solana_net_utils::multi_bind_in_range_with_config(
        ip_addr,
        (port, port + num_sockets as u16),
        SocketConfig::default().reuseport(true),
        num_sockets,
    )
    .unwrap();

    let mut addr = SocketAddr::new(ip_addr, 0);
    let mut read_channels = Vec::new();
    let mut read_threads = Vec::new();
    let recycler = PacketBatchRecycler::default();
    let exit = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StreamerReceiveStats::new("bench-streamer-test"));

    for read in read_sockets {
        read.set_read_timeout(Some(Duration::new(1, 0))).unwrap();

        addr = read.local_addr().unwrap();
        let (s_reader, r_reader) = unbounded();
        read_channels.push(r_reader);
        read_threads.push(receiver(
            "solRcvrBenStrmr".to_string(),
            Arc::new(read),
            exit.clone(),
            s_reader,
            recycler.clone(),
            stats.clone(),
            Some(Duration::from_millis(1)), // coalesce
            true,
            None,
            false,
        ));
    }

    let producer_threads: Vec<_> = (0..num_producers)
        .map(|_| producer(&addr, exit.clone()))
        .collect();

    let rvs = Arc::new(AtomicUsize::new(0));
    let sink_threads: Vec<_> = read_channels
        .into_iter()
        .map(|r_reader| sink(exit.clone(), rvs.clone(), r_reader))
        .collect();
    let start = SystemTime::now();
    let start_val = rvs.load(Ordering::Relaxed);

    sleep(duration);

    let elapsed = start.elapsed().unwrap();
    let end_val = rvs.load(Ordering::Relaxed);
    let time = elapsed.as_secs() * 10_000_000_000 + u64::from(elapsed.subsec_nanos());
    let ftime = (time as f64) / 10_000_000_000_f64;
    let fcount = (end_val - start_val) as f64;
    println!("performance: {:?}", fcount / ftime);
    exit.store(true, Ordering::Relaxed);

    for t_reader in read_threads {
        t_reader.join()?;
    }

    let mut num_packets_sent = 0;
    for t_producer in producer_threads {
        num_packets_sent += t_producer.join()?;
    }
    println!("{num_packets_sent} total packets sent");

    for t_sink in sink_threads {
        t_sink.join()?;
    }
    Ok(())
}
