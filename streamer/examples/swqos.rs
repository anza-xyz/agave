#![allow(clippy::arithmetic_side_effects)]
//! Standalone QUIC streamer server.
//!
//! This utility isolates the QUIC server component, making it convenient for
//! testing and performance tuning. It logs all received "transactions" to a
//! binary file.
//! The logged info includes the bytes 0..32, wherein you can store metadata such
//! as sender's pubkey.

use {
    chrono::{NaiveDate, NaiveDateTime, NaiveTime, Utc},
    clap::Parser,
    crossbeam_channel::bounded,
    log::{debug, info},
    solana_keypair::Keypair,
    solana_net_utils::sockets::{bind_to_with_config, SocketConfiguration},
    solana_pubkey::Pubkey,
    solana_streamer::{
        nonblocking::{quic::SpawnNonBlockingServerResult, swqos::SwQosConfig},
        quic::QuicStreamerConfig,
        streamer::StakedNodes,
    },
    std::{
        collections::HashMap,
        io::{BufRead as _, BufReader, Write},
        net::SocketAddr,
        path::Path,
        str::FromStr as _,
        sync::{Arc, RwLock},
        time::Duration,
    },
    tokio::time::sleep,
    tokio_util::sync::CancellationToken,
};

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseFloatError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs_f64(seconds))
}
const LAMPORTS_PER_SOL: u64 = 1000000000;

pub fn load_staked_nodes_overrides(path: &String) -> anyhow::Result<HashMap<Pubkey, u64>> {
    debug!("Loading staked nodes overrides configuration from {path}");
    if Path::new(&path).exists() {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut map = HashMap::new();
        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 2 {
                anyhow::bail!("invalid line {line_num}: {line}");
            }
            let pubkey = Pubkey::from_str(parts[0])
                .map_err(|_| anyhow::anyhow!("invalid pubkey at line {line_num}"))?;
            let value: u64 = parts[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid number at line {line_num}"))?;

            map.insert(pubkey, value.saturating_mul(LAMPORTS_PER_SOL));
        }
        Ok(map)
    } else {
        anyhow::bail!("Staked nodes overrides provided '{path}' a non-existing file path.")
    }
}

#[derive(Debug, Parser)]
struct Cli {
    #[arg(short, long, default_value_t = 10)]
    max_connections_per_staked_peer: usize,
    #[arg(short, long, default_value_t = 10)]
    max_connections_per_unstaked_peer: usize,

    #[arg(short, long, default_value = "0.0.0.0:8008")]
    bind_to: SocketAddr,

    #[arg(short, long, default_value = "serverlog.bin")]
    log_file: String,

    #[arg(short, long, value_parser = parse_duration)]
    test_duration: Duration,

    #[arg(short, long)]
    stake_amounts: String,
}

// number of threads as in fn default_num_tpu_transaction_forward_receive_threads
#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() -> anyhow::Result<()> {
    agave_logger::setup();
    let cli = Cli::parse();
    let socket = bind_to_with_config(
        cli.bind_to.ip(),
        cli.bind_to.port(),
        SocketConfiguration::default(),
    )
    .expect("should bind");

    let (sender, receiver) = bounded(1024);
    let keypair = Keypair::new();

    let staked_nodes = {
        let nodes = StakedNodes::new(
            Arc::new(HashMap::new()),
            load_staked_nodes_overrides(&cli.stake_amounts)?,
        );
        Arc::new(RwLock::new(nodes))
    };

    let cancel = CancellationToken::new();
    let SpawnNonBlockingServerResult {
        endpoints,
        stats,
        thread: run_thread,
        max_concurrent_connections: _,
    } = solana_streamer::nonblocking::testing_utilities::spawn_stake_weighted_qos_server(
        "quic_streamer_test",
        [socket.try_clone()?],
        &keypair,
        sender,
        staked_nodes,
        QuicStreamerConfig {
            ..QuicStreamerConfig::default()
        },
        SwQosConfig {
            max_connections_per_staked_peer: cli.max_connections_per_staked_peer,
            max_connections_per_unstaked_peer: cli.max_connections_per_unstaked_peer,
            ..Default::default()
        },
        cancel.clone(),
    )?;
    info!("Server listening on {}", socket.local_addr()?);

    let path = cli.log_file.clone();
    let logger_thread = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let solana_epoch = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2020, 3, 16).unwrap(),
            NaiveTime::MIN,
        );
        let logfile = std::fs::File::create(&path)?;
        info!("Logfile in {}", &path);
        let mut logfile = std::io::BufWriter::new(logfile);
        let mut sum = 0;
        for batch in receiver {
            let now = Utc::now().naive_utc();
            let delta_time = (now - solana_epoch).num_microseconds().unwrap() as u64;
            for pkt in batch.iter() {
                let pkt = pkt.to_bytes_packet();
                if pkt.buffer().len() < 32 {
                    continue;
                }
                let pubkey: [u8; 32] = pkt.buffer()[0..32].try_into()?;
                logfile.write_all(&pubkey)?;
                let pkt_len = pkt.buffer().len() as u64;
                logfile.write_all(&pkt_len.to_ne_bytes())?;
                logfile.write_all(&delta_time.to_ne_bytes())?;
                let pubkey = Pubkey::new_from_array(pubkey);
                debug!("{pubkey}: {pkt_len} bytes");
                sum += 1;
            }
        }
        info!("Server captured {sum} TXs");
        logfile.flush()?;
        Ok(())
    });
    // wait for test to finish, report errors early if they occur
    let status = tokio::select! {
        _ = sleep(cli.test_duration)=>{
            info!("Test duration expired");
            Ok(())
        },
        _ = run_thread => {
            Err(anyhow::anyhow!("Server thread exited too early"))
        },
        v = logger_thread => {
            match v {
                Ok(Err(v))=>Err(v.context("Logger thread error")),
                _=>Err(anyhow::anyhow!("Logger thread exited too early"))
            }
        }
    };
    info!("Server terminating");
    cancel.cancel();
    drop(endpoints);
    stats.report("final_stats");
    status
}
