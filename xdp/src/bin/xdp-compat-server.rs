use std::{
    env,
    net::{SocketAddr, UdpSocket},
};

fn usage() -> ! {
    eprintln!("Usage: xdp-compat-server --bind <IP:PORT>");
    std::process::exit(2);
}

fn main() {
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

    let socket = UdpSocket::bind(bind).unwrap_or_else(|e| {
        eprintln!("Failed to bind UDP socket on {bind}: {e}");
        std::process::exit(1);
    });

    println!("Listening on {bind}");

    let mut buf = vec![0u8; 2048];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("recv_from error: {e}");
                continue;
            }
        };
        if let Err(e) = socket.send_to(&buf[..n], peer) {
            eprintln!("send_to error: {e}");
        }
    }
}
