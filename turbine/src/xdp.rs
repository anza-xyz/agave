// re-export since this is needed at validator startup
pub use agave_xdp::{get_cpu, set_cpu_affinity};
#[cfg(target_os = "linux")]
use {
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        load_xdp_program,
        route::Router,
        route_monitor::RouteMonitor,
        tx_loop::{TxLoop, TxLoopBuilder, TxLoopConfigBuilder},
        umem::{OwnedUmem, PageAlignedMemory},
    },
    arc_swap::ArcSwap,
    aya::Ebpf,
    crossbeam_channel::TryRecvError,
    std::{thread::Builder, time::Duration},
};
use {
    crossbeam_channel::{Sender, TrySendError},
    solana_ledger::shred,
    std::{
        error::Error,
        net::{Ipv4Addr, SocketAddr},
        sync::{Arc, atomic::AtomicBool},
        thread,
    },
};

#[cfg(target_os = "linux")]
const ROUTE_MONITOR_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

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

#[cfg(not(target_os = "linux"))]
pub struct XdpRetransmitBuilder {}

#[cfg(target_os = "linux")]
pub struct XdpRetransmitBuilder {
    tx_loops: Vec<TxLoop<OwnedUmem<PageAlignedMemory>>>,
    rtx_channel_cap: usize,
    maybe_ebpf: Option<Ebpf>,
    atomic_router: Arc<ArcSwap<Router>>,
    route_monitor_handle: thread::JoinHandle<()>,
}

impl XdpRetransmitBuilder {
    #[cfg(not(target_os = "linux"))]
    pub fn new(
        _config: XdpConfig,
        _src_port: u16,
        _src_ip: Option<Ipv4Addr>,
        _exit: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn Error>> {
        Err("XDP is only supported on Linux".into())
    }

    #[cfg(target_os = "linux")]
    pub fn new(
        config: XdpConfig,
        src_port: u16,
        src_ip: Option<Ipv4Addr>,
        exit: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn Error>> {
        use {
            caps::{
                CapSet,
                Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
            },
            std::collections::HashSet,
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

        let reserved_cores = cpus.iter().cloned().collect::<HashSet<_>>();
        let available_cores = core_affinity::get_core_ids()
            .expect("linux provide affine cores")
            .into_iter()
            .map(|core_affinity::CoreId { id }| id)
            .collect::<HashSet<_>>();
        let unreserved_cores = available_cores
            .difference(&reserved_cores)
            .cloned()
            .collect::<Vec<_>>();

        let tx_loop_builders = cpus
            .into_iter()
            .zip(std::iter::repeat_with(|| tx_loop_config.clone()))
            .enumerate()
            .map(|(i, (cpu_id, config))| {
                // since we aren't necessarily allocating from the thread that we intend to run on,
                // temporarily switch to the target cpu for each TxLoop to ensure that the Umem region
                // is allocated to the correct numa node
                set_cpu_affinity([cpu_id]).unwrap();
                let tx_loop_builder = TxLoopBuilder::new(cpu_id, QueueId(i as u64), config, &dev);
                // migrate main thread back off of the last xdp reserved cpu
                set_cpu_affinity(unreserved_cores.clone()).unwrap();
                tx_loop_builder
            })
            .collect::<Vec<_>>();

        // switch to higher caps while we setup XDP. We assume that an error in
        // this function is irrecoverable so we don't try to drop on errors.
        caps::raise(None, CapSet::Effective, CAP_NET_ADMIN)
            .expect("raise CAP_NET_ADMIN capability");
        caps::raise(None, CapSet::Effective, CAP_NET_RAW).expect("raise CAP_NET_RAW capability");

        let maybe_ebpf_result = if zero_copy {
            caps::raise(None, CapSet::Effective, CAP_BPF).expect("raise CAP_BPF capability");
            caps::raise(None, CapSet::Effective, CAP_PERFMON)
                .expect("raise CAP_PERFMON capability");

            let load_result =
                load_xdp_program(&dev).map_err(|e| format!("failed to attach xdp program: {e}"));

            caps::drop(None, CapSet::Effective, CAP_PERFMON).expect("drop CAP_PERFMON capability");
            caps::drop(None, CapSet::Effective, CAP_BPF).expect("drop CAP_BPF capability");

            Some(load_result)
        } else {
            None
        };

        let tx_loops = tx_loop_builders
            .into_iter()
            .map(|tx_loop_builder| tx_loop_builder.build())
            .collect::<Vec<_>>();

        let router_result = Router::new();

        caps::drop(None, CapSet::Effective, CAP_NET_RAW).expect("drop CAP_NET_RAW capability");
        caps::drop(None, CapSet::Effective, CAP_NET_ADMIN).expect("drop CAP_NET_ADMIN capability");

        let mut router = router_result?;
        router.build_caches()?;

        // Use ArcSwap for lock-free updates of the routing table
        let atomic_router = Arc::new(ArcSwap::from_pointee(router));
        let route_monitor_handle = RouteMonitor::start(
            Arc::clone(&atomic_router),
            exit.clone(),
            ROUTE_MONITOR_UPDATE_INTERVAL,
            || {
                // we need to retain CAP_NET_ADMIN in case the netlink socket needs reinitialized
                let retained_caps = caps::CapsHashSet::from_iter([caps::Capability::CAP_NET_ADMIN]);
                caps::set(None, caps::CapSet::Permitted, &retained_caps)
                    .expect("linux allows permitted capset to be set");
                info!("route monitor thread started");
            },
        );

        let maybe_ebpf = maybe_ebpf_result.transpose()?;

        Ok(Self {
            tx_loops,
            rtx_channel_cap,
            maybe_ebpf,
            atomic_router,
            route_monitor_handle,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn build(self) -> (XdpRetransmitter, XdpSender) {
        (
            XdpRetransmitter { threads: vec![] },
            XdpSender { senders: vec![] },
        )
    }

    #[cfg(target_os = "linux")]
    pub fn build(self) -> (XdpRetransmitter, XdpSender) {
        const DROP_CHANNEL_CAP: usize = 1_000_000;

        let Self {
            tx_loops,
            rtx_channel_cap,
            maybe_ebpf,
            atomic_router,
            route_monitor_handle,
        } = self;

        let (drop_sender, drop_receiver) = crossbeam_channel::bounded(DROP_CHANNEL_CAP);
        let mut threads = vec![route_monitor_handle];

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

        let mut senders = vec![];
        for (i, tx_loop) in tx_loops.into_iter().enumerate() {
            let (sender, receiver) = crossbeam_channel::bounded(rtx_channel_cap);
            let drop_sender = drop_sender.clone();
            let atomic_router = Arc::clone(&atomic_router);
            threads.push(
                Builder::new()
                    .name(format!("solRetransmIO{i:02}"))
                    .spawn(move || {
                        tx_loop.run(receiver, drop_sender, move |ip| {
                            let r = atomic_router.load();
                            r.route(*ip).ok()
                        })
                    })
                    .unwrap(),
            );
            senders.push(sender);
        }

        (XdpRetransmitter { threads }, XdpSender { senders })
    }
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
