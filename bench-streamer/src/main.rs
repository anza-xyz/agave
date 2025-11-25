#![allow(clippy::arithmetic_side_effects)]

use {
    clap::{crate_description, crate_name, value_t_or_exit, Arg, Command},
    crossbeam_channel::unbounded,
    solana_net_utils::{
        bind_to_unspecified,
        sockets::{multi_bind_in_range_with_config, SocketConfiguration},
    },
    solana_streamer::{
        packet::{Packet, PacketBatchRecycler, RecycledPacketBatch, PACKET_DATA_SIZE},
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

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn producer(dest_addr: &SocketAddr, exit: Arc<AtomicBool>) -> JoinHandle<usize> {
    let send = bind_to_unspecified().unwrap();

    let batch_size = 1024;
    let payload = [0u8; PACKET_DATA_SIZE];
    let packet = {
        let mut packet = Packet::default();
        packet.buffer_mut()[..payload.len()].copy_from_slice(&payload);
        packet.meta_mut().size = payload.len();
        packet.meta_mut().set_socket_addr(dest_addr);
        packet
    };
    let mut packet_batch = RecycledPacketBatch::with_capacity(batch_size);
    packet_batch.resize(batch_size, packet);

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
                .default_value("4")
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
    let num_producers = value_t_or_exit!(matches, "num-producers", usize);
    let duration_secs = value_t_or_exit!(matches, "duration", u64);
    let duration = Duration::new(duration_secs, 0);

    let port = 0;
    let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let (_port, read_sockets) = multi_bind_in_range_with_config(
        ip_addr,
        (port, port + num_sockets as u16),
        SocketConfiguration::default(),
        num_sockets,
    )
    .unwrap();

    let mut addr = SocketAddr::new(ip_addr, 0);
    let recycler = PacketBatchRecycler::default();
    let exit = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StreamerReceiveStats::new("bench-streamer-test"));

    let (read_threads, read_channels): (Vec<_>, Vec<_>) = read_sockets
        .into_iter()
        .map(|read_socket| {
            read_socket
                .set_read_timeout(Some(Duration::new(1, 0)))
                .unwrap();
            addr = read_socket.local_addr().unwrap();

            let (packet_sender, packet_receiver) = unbounded();
            let receiver = receiver(
                "solRcvrBenStrmr".to_string(),
                Arc::new(read_socket),
                exit.clone(),
                packet_sender,
                recycler.clone(),
                stats.clone(),
                None, // coalesce
                true,
                None,
                false,
            );

            (receiver, packet_receiver)
        })
        .unzip();

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
