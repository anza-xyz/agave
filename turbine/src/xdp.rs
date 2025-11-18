// re-export since this is needed at validator startup
pub use agave_xdp::set_cpu_affinity;
#[cfg(target_os = "linux")]
use {
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        load_xdp_program,
        tx_loop::{PartialTxLoop, TxLoop, TxLoopConfigBuilder},
        umem::{OwnedUmem, PageAlignedMemory},
    },
    aya::Ebpf,
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

pub struct PartialXdpRetransmitter {
    tx_loops: Vec<TxLoop<OwnedUmem<PageAlignedMemory>>>,
    rtx_channel_cap: usize,
    maybe_ebpf: Option<Ebpf>,
}

impl PartialXdpRetransmitter {
    #[cfg(not(target_os = "linux"))]
    pub fn new(
        _config: XdpConfig,
        _src_port: u16,
        _src_ip: Option<Ipv4Addr>,
    ) -> Result<Self, Box<dyn Error>> {
        Err("XDP is only supported on Linux".into())
    }

    #[cfg(target_os = "linux")]
    pub fn new(
        config: XdpConfig,
        src_port: u16,
        src_ip: Option<Ipv4Addr>,
    ) -> Result<Self, Box<dyn Error>> {
        use caps::{
            CapSet,
            Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
        };
        let XdpConfig {
            interface: maybe_interface,
            cpus,
            zero_copy,
            rtx_channel_cap,
        } = config;

        let dev = Arc::new(if let Some(interface) = maybe_interface {
            NetworkDevice::new(interface).unwrap()
        } else {
            NetworkDevice::new_from_default_route().unwrap()
        });

        let mut tx_loop_config_builder = TxLoopConfigBuilder::new(src_port);
        tx_loop_config_builder.zero_copy(zero_copy);
        if let Some(src_ip) = src_ip {
            tx_loop_config_builder.override_src_ip(src_ip);
        }
        let tx_loop_config = tx_loop_config_builder.build_with_src_device(&dev);

        let partial_tx_loops = cpus
            .into_iter()
            .zip(std::iter::repeat_with(|| tx_loop_config.clone()))
            .enumerate()
            .map(|(i, (cpu_id, config))| {
                PartialTxLoop::new(cpu_id, QueueId(i as u64), config, &dev)
            })
            .collect::<Vec<_>>();

        // switch to higher caps while we setup XDP. We assume that an error in
        // this function is irrecoverable so we don't try to drop on errors.
        for cap in [CAP_NET_ADMIN, CAP_NET_RAW] {
            caps::raise(None, CapSet::Effective, cap)
                .map_err(|e| format!("failed to raise {cap:?} capability: {e}"))?;
        }
        let maybe_ebpf = if zero_copy {
            for cap in [CAP_BPF, CAP_PERFMON] {
                caps::raise(None, CapSet::Effective, cap)
                    .map_err(|e| format!("failed to raise {cap:?} capability: {e}"))?;
            }
            let load_result =
                load_xdp_program(&dev).map_err(|e| format!("failed to attach xdp program: {e}"));
            for cap in [CAP_BPF, CAP_PERFMON] {
                caps::drop(None, CapSet::Effective, cap).unwrap();
            }
            Some(load_result?)
        } else {
            None
        };

        let tx_loops = partial_tx_loops
            .into_iter()
            .map(|partial_tx_loop| partial_tx_loop.finish())
            .collect::<Vec<_>>();

        for cap in [CAP_NET_ADMIN, CAP_NET_RAW] {
            caps::drop(None, CapSet::Effective, cap).unwrap();
        }

        Ok(Self {
            tx_loops,
            rtx_channel_cap,
            maybe_ebpf,
        })
    }

    pub fn finish(self) -> (XdpRetransmitter, XdpSender) {
        const DROP_CHANNEL_CAP: usize = 1_000_000;

        let PartialXdpRetransmitter {
            tx_loops,
            rtx_channel_cap,
            maybe_ebpf,
        } = self;

        let (senders, receivers) = (0..tx_loops.len())
            .map(|_| crossbeam_channel::bounded(rtx_channel_cap))
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
                    drop(maybe_ebpf);
                })
                .unwrap(),
        );

        for (i, (receiver, tx_loop)) in receivers.into_iter().zip(tx_loops.into_iter()).enumerate()
        {
            let drop_sender = drop_sender.clone();
            threads.push(
                Builder::new()
                    .name(format!("solRetransmIO{i:02}"))
                    .spawn(move || tx_loop.run(receiver, drop_sender))
                    .unwrap(),
            );
        }

        (XdpRetransmitter { threads }, XdpSender { senders })
    }
}

pub struct XdpRetransmitter {
    threads: Vec<thread::JoinHandle<()>>,
}

impl XdpRetransmitter {
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
