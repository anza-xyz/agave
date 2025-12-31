use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub struct Surfpool {
    tx: mpsc::Sender<()>,
    pub url: String,
    handle: Option<thread::JoinHandle<()>>,
}

impl Surfpool {
    // Port 8899 is the default, but we let it be configurable
    pub fn new(port: u16) -> Self {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        
        let addr = format!("127.0.0.1:{}", port);
        let url = format!("http://{}", addr);

        let thread_addr = addr.clone();
        let jh = thread::spawn(move || {
            let mut child = Command::new("surfpool")
                .args(["start", "--port", &port.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::inherit()) // Show errors in console if it fails
                .spawn()
                .expect("failed to start surfpool");

            // Wait for port to open
            let now = Instant::now();
            let mut is_ready = false;
            while now.elapsed() < Duration::from_secs(5) {
                if TcpStream::connect(&thread_addr).is_ok() {
                    is_ready = true;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }

            if is_ready {
                let _ = ready_tx.send(());
            }

            // Hang out until we get the signal to quit
            let _ = rx.recv();
            let _ = child.kill();
            let _ = child.wait();
        });

        // Block until the port is actually listening
        ready_rx.recv_timeout(Duration::from_secs(5))
            .expect("Surfpool didn't start in time");

        Self {
            tx,
            url,
            handle: Some(jh),
        }
    }
}

impl Drop for Surfpool {
    fn drop(&mut self) {
        let _ = self.tx.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::nonblocking::rpc_client::RpcClient;

    #[tokio::test]
    async fn test_basic_connection() {
        let svc = Surfpool::new(8899);
        let client = RpcClient::new(svc.url.clone());
        
        let v = client.get_version().await.unwrap();
        println!("Connected: {}", v.solana_core);
    }
}