// re-export since this is needed at validator startup
pub use agave_xdp::set_cpu_affinity;
#[cfg(target_os = "linux")]
use {
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        load_xdp_program,
        tx_loop::tx_loop,
    },
    crossbeam_channel::TryRecvError,
    std::{sync::Arc, thread::Builder, time::Duration},
};
use {
    crossbeam_channel::{Sender, TrySendError},
    solana_ledger::shred,
    std::{
        error::Error,
        net::{Ipv4Addr, SocketAddr},
        thread,
    },
};

#[derive(Clone, Debug)]
pub struct XdpConfig {
    pub interface: Option<String>,
    pub cpus: Vec<usize>,
    pub zero_copy: bool,
    // The capacity of the channel that sits between retransmit stage and each XDP thread that
    // enqueues packets to the NIC.
    pub rtx_channel_cap: usize,
}

impl XdpConfig {
    // A nice round number
    const DEFAULT_RTX_CHANNEL_CAP: usize = 1_000_000;
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            interface: None,
            cpus: vec![],
            zero_copy: false,
            rtx_channel_cap: Self::DEFAULT_RTX_CHANNEL_CAP,
        }
    }
}

impl XdpConfig {
    pub fn new(interface: Option<impl Into<String>>, cpus: Vec<usize>, zero_copy: bool) -> Self {
        Self {
            interface: interface.map(|s| s.into()),
            cpus,
            zero_copy,
            rtx_channel_cap: XdpConfig::DEFAULT_RTX_CHANNEL_CAP,
        }
    }
}

#[derive(Clone)]
pub struct XdpSender {
    senders: Vec<Sender<(XdpAddrs, shred::Payload)>>,
}

pub enum XdpAddrs {
    Single(SocketAddr),
    Multi(Vec<SocketAddr>),
}

impl From<SocketAddr> for XdpAddrs {
    #[inline]
    fn from(addr: SocketAddr) -> Self {
        XdpAddrs::Single(addr)
    }
}

impl From<Vec<SocketAddr>> for XdpAddrs {
    #[inline]
    fn from(addrs: Vec<SocketAddr>) -> Self {
        XdpAddrs::Multi(addrs)
    }
}

impl AsRef<[SocketAddr]> for XdpAddrs {
    #[inline]
    fn as_ref(&self) -> &[SocketAddr] {
        match self {
            XdpAddrs::Single(addr) => std::slice::from_ref(addr),
            XdpAddrs::Multi(addrs) => addrs,
        }
    }
}

impl XdpSender {
    #[inline]
    pub(crate) fn try_send(
        &self,
        sender_index: usize,
        addr: impl Into<XdpAddrs>,
        payload: shred::Payload,
    ) -> Result<(), TrySendError<(XdpAddrs, shred::Payload)>> {
        self.senders[sender_index % self.senders.len()].try_send((addr.into(), payload))
    }
}

pub struct XdpRetransmitter {
    threads: Vec<thread::JoinHandle<()>>,
}

impl XdpRetransmitter {
    #[cfg(not(target_os = "linux"))]
    pub fn new(
        _config: XdpConfig,
        _src_port: u16,
        _src_ip: Option<Ipv4Addr>,
    ) -> Result<(Self, XdpSender), Box<dyn Error>> {
        Err("XDP is only supported on Linux".into())
    }

    #[cfg(target_os = "linux")]
    pub fn new(
        config: XdpConfig,
        src_port: u16,
        src_ip: Option<Ipv4Addr>,
    ) -> Result<(Self, XdpSender), Box<dyn Error>> {
        use caps::{
            CapSet,
            Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
        };
        const DROP_CHANNEL_CAP: usize = 1_000_000;

        // switch to higher caps while we setup XDP. We assume that an error in
        // this function is irrecoverable so we don't try to drop on errors.
        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
            caps::raise(None, CapSet::Effective, cap)
                .map_err(|e| format!("failed to raise {cap:?} capability: {e}"))?;
        }

        let dev = Arc::new(if let Some(interface) = config.interface {
            NetworkDevice::new(interface).unwrap()
        } else {
            NetworkDevice::new_from_default_route().unwrap()
        });

        let ebpf = if config.zero_copy {
            Some(load_xdp_program(&dev).map_err(|e| format!("failed to attach xdp program: {e}"))?)
        } else {
            None
        };

        for cap in [CAP_NET_ADMIN, CAP_NET_RAW, CAP_BPF, CAP_PERFMON] {
            caps::drop(None, CapSet::Effective, cap).unwrap();
        }

        let (senders, receivers) = (0..config.cpus.len())
            .map(|_| crossbeam_channel::bounded(config.rtx_channel_cap))
            .unzip::<_, _, Vec<_>, Vec<_>>();

        let mut threads = vec![];

        let (drop_sender, drop_receiver) = crossbeam_channel::bounded(DROP_CHANNEL_CAP);
        threads.push(
            Builder::new()
                .name("solRetransmDrop".to_owned())
                .spawn(move || {
                    loop {
                        // drop shreds in a dedicated thread so that we never lock/madvise() from
                        // the xdp thread
                        match drop_receiver.try_recv() {
                            Ok(i) => {
                                drop(i);
                            }
                            Err(TryRecvError::Empty) => {
                                thread::sleep(Duration::from_millis(1));
                            }
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                    // move the ebpf program here so it stays attached until we exit
                    drop(ebpf);
                })
                .unwrap(),
        );

        for (i, (receiver, cpu_id)) in receivers
            .into_iter()
            .zip(config.cpus.into_iter())
            .enumerate()
        {
            let dev = Arc::clone(&dev);
            let drop_sender = drop_sender.clone();
            threads.push(
                Builder::new()
                    .name(format!("solRetransmIO{i:02}"))
                    .spawn(move || {
                        tx_loop(
                            cpu_id,
                            &dev,
                            QueueId(i as u64),
                            config.zero_copy,
                            None,
                            src_ip,
                            src_port,
                            None,
                            receiver,
                            drop_sender,
                        )
                    })
                    .unwrap(),
            );
        }

        Ok((Self { threads }, XdpSender { senders }))
    }

    pub fn join(self) -> thread::Result<()> {
        for handle in self.threads {
            handle.join()?;
        }
        Ok(())
    }
}

/// Returns the IPv4 address of the master interface if the given interface is part of a bond.
#[cfg(target_os = "linux")]
pub fn master_ip_if_bonded(interface: &str) -> Option<Ipv4Addr> {
    let master_ifindex_path = format!("/sys/class/net/{interface}/master/ifindex");
    if let Ok(contents) = std::fs::read_to_string(&master_ifindex_path) {
        let idx = contents.trim().parse().unwrap();
        return Some(
            NetworkDevice::new_from_index(idx)
                .and_then(|dev| dev.ipv4_addr())
                .unwrap_or_else(|e| {
                    panic!(
                        "failed to open bond master interface for {interface}: master index \
                         {idx}: {e}"
                    )
                }),
        );
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn master_ip_if_bonded(_interface: &str) -> Option<Ipv4Addr> {
    None
}
