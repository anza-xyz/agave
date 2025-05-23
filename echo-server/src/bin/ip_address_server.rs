use {
    clap::Parser,
    std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
};

#[derive(Parser)]
#[command(version, name="solana-ip-address-server", about, long_about = None)]
struct Cli {
    #[arg()]
    /// TCP port to bind to
    port: u16,

    /// Shred Version to be advertised.
    #[arg(short, long)]
    shred_version: Option<u16>,

    /// Bind address
    #[arg(short, long, default_value_t=IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    bind_address: IpAddr,

    /// Number of threads
    #[arg(short, long)]
    threads: usize,
}

fn main() {
    solana_logger::setup();
    let cli = Cli::parse();
    let bind_addr = SocketAddr::from((cli.bind_address, cli.port));
    let tcp_listener = TcpListener::bind(bind_addr).expect("unable to bind to port specified");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("solIpEchoSrvrRt")
        .worker_threads(cli.threads)
        .enable_all()
        .build()
        .expect("new tokio runtime");
    runtime.block_on(solana_echo_server::ip_echo_server(
        tcp_listener,
        /*shred_version=*/ cli.shred_version,
    ));
}
