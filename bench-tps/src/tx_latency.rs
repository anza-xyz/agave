use std::thread::Builder;
use {
    crossbeam_channel::Receiver,
    solana_sdk::signature::Signature,
    std::{
        thread::{sleep, JoinHandle},
        time::Duration,
    },
};

pub(crate) enum HttpOrWs {
    Http,
    Ws,
}

pub(crate) fn measure_tx_latency(
    receiver: Receiver<Signature>,
    http_or_ws: HttpOrWs,
    http_sleep_ms: u64,
) -> JoinHandle<()> {
    Builder::new()
        .name("measureTxLatency".to_string())
        .spawn(move || loop {
            let signature = match receiver.recv() {
                Ok(s) => s,
                Err(_) => break,
            };
            println!("Got transaction signature: {}", signature);
            sleep(Duration::from_millis(http_sleep_ms));
        })
        .expect("measureTxLatency should have started successfully.")
}
