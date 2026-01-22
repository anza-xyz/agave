use {
    log::{error, info},
    solana_net_utils::sockets::bind_to,
    std::{env, net::SocketAddr, process::exit},
    xdp_compatability::hash_response,
};

fn usage() -> ! {
    error!("Usage: server --bind <IP:PORT>");
    exit(1);
}

fn main() {
    agave_logger::setup_with_default("info");
    let mut args = env::args().skip(1);
    let mut bind = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next(),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }

    let bind = bind
        .and_then(|s| s.parse::<SocketAddr>().ok())
        .unwrap_or_else(|| usage());

    let socket = bind_to(bind.ip(), bind.port()).unwrap_or_else(|e| {
        error!("Failed to bind UDP socket on {bind}: {e}");
        exit(1);
    });

    info!("Listening on {bind}");

    let mut buf = vec![0u8; 2048];
    let mut hash_buf = [0u8; 32];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                error!("recv_from error: {e}");
                continue;
            }
        };
        // For agave-xdp requests, reply with SHA-256 hash of the token so the client can verify.
        let to_send: &[u8] = match hash_response(&buf, n) {
            Some(hash) => {
                hash_buf.copy_from_slice(&hash);
                &hash_buf[..]
            }
            None => &buf[..n],
        };
        if let Err(e) = socket.send_to(to_send, peer) {
            error!("send_to error: {e}");
        }
    }
}
