use {
    agave_xdp::{device::NetworkDevice, load_xdp_program},
    agave_xdp_ebpf::FirewallConfig,
    clap::Parser,
    std::time::Duration,
};

#[derive(Debug, clap::Parser)]
struct Cli {
    #[arg(short, long)]
    interface: String,
    #[arg(short, long)]
    config: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    solana_logger::setup_with_default_filter();

    let cli = Cli::parse();
    let interface = &cli.interface;
    let dev = NetworkDevice::new(interface).unwrap();

    let firewall_config: FirewallConfig =
        serde_json::from_reader(std::fs::File::open(&cli.config).unwrap()).unwrap();
    let ebpf = load_xdp_program(&dev, Some(firewall_config)).unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    drop(ebpf);
}
