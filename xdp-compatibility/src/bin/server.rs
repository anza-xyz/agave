use {
    clap::{crate_description, crate_name, value_t_or_exit, App, Arg},
    log::{error, info, warn},
    solana_net_utils::sockets::bind_to,
    std::{net::SocketAddr, process::exit},
    xdp_compatibility::{response_for_request, RESPONSE_LEN},
};

fn get_clap_app<'ab, 'v>() -> App<'ab, 'v> {
    App::new(crate_name!()).about(crate_description!()).arg(
        Arg::with_name("bind_address")
            .long("bind-address")
            .value_name("IP:PORT")
            .takes_value(true)
            .required(true)
            .help("Address to bind the UDP echo server to"),
    )
}

fn parse_args() -> SocketAddr {
    let matches = get_clap_app().get_matches();
    value_t_or_exit!(matches, "bind_address", SocketAddr)
}

fn main() {
    agave_logger::setup_with_default("info");
    let bind_address = parse_args();

    let socket = bind_to(bind_address.ip(), bind_address.port()).unwrap_or_else(|e| {
        error!("Failed to bind UDP socket on {bind_address}: {e}");
        exit(1);
    });

    info!("Listening on {bind_address}");

    let mut buf = vec![0u8; 2048];
    let mut response_buf = [0u8; RESPONSE_LEN];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                error!("recv_from error: {e}");
                continue;
            }
        };
        // For agave-xdp requests, reply with magic + seq + hash so the client can verify.
        let Some(response) = response_for_request(&buf, n) else {
            warn!("Ignoring non-agave-xdp packet from {peer} (len {n})");
            continue;
        };
        response_buf.copy_from_slice(&response);
        if let Err(e) = socket.send_to(&response_buf, peer) {
            error!("send_to error: {e}");
        }
    }
}
