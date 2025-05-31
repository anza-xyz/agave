use {clap::Parser, std::net::SocketAddr};

#[derive(Parser)]
#[command(version, name="solana-ip-address", about, long_about = None)]
struct Cli {
    #[arg(value_parser=solana_net_utils::parse_host_port)]
    /// Host:port to connect to
    addr: SocketAddr,
}

fn main() {
    solana_logger::setup();
    let cli = Cli::parse();
    match solana_net_utils::get_public_ip_addr(&cli.addr) {
        Ok(ip) => println!("{ip}"),
        Err(err) => {
            eprintln!("{}: {err}", &cli.addr);
            std::process::exit(1)
        }
    }
}
