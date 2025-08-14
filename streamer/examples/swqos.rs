use {
    clap::Parser,
    crossbeam_channel::bounded,
    log::{debug, info},
    solana_keypair::Keypair,
    solana_net_utils::sockets::{bind_to_with_config, SocketConfiguration},
    solana_pubkey::Pubkey,
    solana_streamer::{
        quic::{spawn_server, QuicServerParams, SpawnServerResult},
        streamer::StakedNodes,
    },
    std::{
        collections::HashMap,
        io::{BufRead as _, BufReader, Write},
        net::SocketAddr,
        path::Path,
        str::FromStr as _,
        sync::{atomic::AtomicBool, Arc, RwLock},
        time::Duration,
    },
    tokio::time::Instant,
};

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseFloatError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs_f64(seconds))
}
const LAMPORTS_PER_SOL: u64 = 1000000000;

pub fn load_staked_nodes_overrides(path: &String) -> anyhow::Result<HashMap<Pubkey, u64>> {
    debug!("Loading staked nodes overrides configuration from {}", path);
    if Path::new(&path).exists() {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut map = HashMap::new();
        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 2 {
                anyhow::bail!("invalid line {}: {}", line_num, line);
            }
            let pubkey = Pubkey::from_str(parts[0])
                .map_err(|_| anyhow::anyhow!("invalid pubkey at line {}", line_num))?;
            let value: u64 = parts[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid number at line {}", line_num))?;

            map.insert(pubkey, value.saturating_mul(LAMPORTS_PER_SOL));
        }
        Ok(map)
    } else {
        anyhow::bail!("Staked nodes overrides provided '{path}' a non-existing file path.")
    }
}

#[derive(Debug, Parser)]
struct Cli {
    #[arg(short, long, default_value_t = 1)]
    max_connections_per_peer: usize,
    #[arg(short, long, default_value = "0.0.0.0:8008")]
    bind_to: SocketAddr,

    #[arg(short, long, value_parser = parse_duration)]
    test_duration: Duration,

    #[arg(short, long)]
    stake_amounts: String,
}

pub fn main() {
    solana_logger::setup();
    let cli = Cli::parse();
    let socket = bind_to_with_config(
        cli.bind_to.ip(),
        cli.bind_to.port(),
        SocketConfiguration::default(),
    )
    .expect("should bind");

    let exit = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = bounded(1024);
    let keypair = Keypair::new();

    let staked_nodes = {
        let nodes = StakedNodes::new(
            Arc::new(HashMap::new()),
            load_staked_nodes_overrides(&cli.stake_amounts).unwrap(),
        );
        Arc::new(RwLock::new(nodes))
    };

    let SpawnServerResult {
        endpoints,
        thread,
        key_updater: _,
    } = spawn_server(
        "solQuicTest",
        "quic_streamer_test",
        [socket.try_clone().unwrap()],
        &keypair,
        sender,
        exit.clone(),
        staked_nodes,
        QuicServerParams {
            max_connections_per_peer: cli.max_connections_per_peer,
            ..QuicServerParams::default()
        },
    )
    .unwrap();
    info!("Server listening on {}", socket.local_addr().unwrap());

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let start = Instant::now();
            let path = "./results/serverlog.bin";
            let logfile = std::fs::File::create(path).unwrap();
            info!("Logfile in {}", path);
            let mut logfile = std::io::BufWriter::new(logfile);
            for batch in receiver {
                let delta_time = start.elapsed().as_micros() as u32;
                for pkt in batch.iter() {
                    let pkt = pkt.to_bytes_packet();
                    let pubkey: [u8; 32] = pkt.buffer()[16..16 + 32].try_into().unwrap();
                    logfile.write_all(&pubkey).unwrap();
                    let pkt_len = pkt.buffer().len();
                    logfile.write_all(&pkt_len.to_ne_bytes()).unwrap();
                    logfile.write_all(&delta_time.to_ne_bytes()).unwrap();
                    let pubkey = Pubkey::new_from_array(pubkey);
                    debug!("{pubkey}: {pkt_len} bytes");
                }
            }
            logfile.flush().unwrap();
        });

        std::thread::sleep(cli.test_duration);
        exit.store(true, std::sync::atomic::Ordering::Relaxed);
        drop(endpoints);
        thread.join().unwrap();
    });
    info!("Server terminating");
}
