//! Responsible for monitoring the heartbeat of the Vortexor
//! And take actions if timeout has been dectected.
//! The timeout value is configurable. When timeout, the monitor will
//! fallback to the built-in TPU receiver.
//! The monitor receive hearbeat messages through a crossbeam channel.
//! The heartbeat messages are sent by the Vortexor and forwarded by the
//! Vortexor receiver adapter.

use {
    crossbeam_channel::Receiver,
    log::{error, info},
    std::{
        net::SocketAddr,
        thread,
        time::{Duration, Instant},
    },
};

/// Configuration for the heartbeat monitor.
pub struct HeartbeatMonitorConfig {
    pub timeout: Duration,
}

/// Heartbeat monitor service.
pub struct HeartbeatMonitor {
    config: HeartbeatMonitorConfig,
    heartbeat_receiver: Receiver<SocketAddr>,
}

impl HeartbeatMonitor {
    /// Creates a new HeartbeatMonitor.
    pub fn new(config: HeartbeatMonitorConfig, heartbeat_receiver: Receiver<SocketAddr>) -> Self {
        Self {
            config,
            heartbeat_receiver,
        }
    }

    /// Starts the heartbeat monitor service.
    /// If a timeout is detected, it will trigger a fallback action.
    pub fn start<F>(&self, fallback_action: F)
    where
        F: Fn() + Send + 'static,
    {
        let timeout = self.config.timeout;
        let heartbeat_receiver = self.heartbeat_receiver.clone();

        thread::spawn(move || {
            let mut last_heartbeat = Instant::now();

            loop {
                match heartbeat_receiver.recv_timeout(timeout) {
                    Ok(source_addr) => {
                        // Received a heartbeat, update the last heartbeat timestamp.
                        last_heartbeat = Instant::now();
                        info!(
                            "Heartbeat received from {}. Resetting timeout.",
                            source_addr
                        );
                    }
                    Err(_) => {
                        // Timeout occurred, trigger the fallback action.
                        if last_heartbeat.elapsed() >= timeout {
                            error!("Heartbeat timeout detected. Triggering fallback action.");
                            fallback_action();
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::unbounded,
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr},
            sync::{Arc, Mutex},
            time::Duration,
        },
    };

    #[test]
    fn test_heartbeat_monitor_receives_heartbeat() {
        let (sender, receiver) = unbounded();
        let config = HeartbeatMonitorConfig {
            timeout: Duration::from_secs(2),
        };
        let monitor = HeartbeatMonitor::new(config, receiver);

        let fallback_triggered = Arc::new(Mutex::new(false));
        let fallback_triggered_clone = Arc::clone(&fallback_triggered);

        monitor.start(move || {
            let mut triggered = fallback_triggered_clone.lock().unwrap();
            *triggered = true;
        });

        // Send a heartbeat and ensure no fallback is triggered.
        let source_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        sender.send(source_addr).unwrap();
        thread::sleep(Duration::from_secs(1));

        let triggered = fallback_triggered.lock().unwrap();
        assert!(
            !*triggered,
            "Fallback should not be triggered when heartbeat is received."
        );
    }

    #[test]
    fn test_heartbeat_monitor_triggers_fallback_on_timeout() {
        let (_sender, receiver) = unbounded();
        let config = HeartbeatMonitorConfig {
            timeout: Duration::from_secs(1),
        };
        let monitor = HeartbeatMonitor::new(config, receiver);

        let fallback_triggered = Arc::new(Mutex::new(false));
        let fallback_triggered_clone = Arc::clone(&fallback_triggered);

        monitor.start(move || {
            let mut triggered = fallback_triggered_clone.lock().unwrap();
            *triggered = true;
        });

        // Do not send a heartbeat and wait for the timeout.
        thread::sleep(Duration::from_secs(2));

        let triggered = fallback_triggered.lock().unwrap();
        assert!(*triggered, "Fallback should be triggered on timeout.");
    }

    #[test]
    fn test_heartbeat_monitor_resets_on_heartbeat() {
        let (sender, receiver) = unbounded();
        let config = HeartbeatMonitorConfig {
            timeout: Duration::from_secs(1),
        };
        let monitor = HeartbeatMonitor::new(config, receiver);

        let fallback_triggered = Arc::new(Mutex::new(false));
        let fallback_triggered_clone = Arc::clone(&fallback_triggered);

        monitor.start(move || {
            let mut triggered = fallback_triggered_clone.lock().unwrap();
            *triggered = true;
        });

        // Send heartbeats periodically to prevent timeout.
        let source_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        for _ in 0..3 {
            sender.send(source_addr).unwrap();
            thread::sleep(Duration::from_millis(500));
        }

        let triggered = fallback_triggered.lock().unwrap();
        assert!(
            !*triggered,
            "Fallback should not be triggered when heartbeats are received periodically."
        );
    }
}
