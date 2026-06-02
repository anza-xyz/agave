use {
    crate::ecn_codepoint::EcnCodepoint,
    bytes::Bytes,
    crossbeam_channel::{Sender, TrySendError},
    std::{
        error::Error,
        net::{SocketAddr, SocketAddrV4},
        sync::{Arc, atomic::AtomicBool},
        thread,
    },
};
#[cfg(target_os = "linux")]
use {
    crate::{
        device::{NetworkDevice, QueueId},
        load_xdp_program,
        route::{RouteTable, Router, RoutingTables},
        route_monitor::RouteMonitor,
        set_cpu_affinity,
        tx_loop::{TxLoop, TxLoopBuilder, TxLoopConfigBuilder, TxPacket},
        umem::{OwnedUmem, PageAlignedMemory},
    },
    arc_swap::ArcSwap,
    arrayvec::ArrayVec,
    aya::Ebpf,
    crossbeam_channel::TryRecvError,
    log::info,
    std::{
        net::{IpAddr, Ipv4Addr},
        thread::Builder,
        time::Duration,
    },
};

#[cfg(target_os = "linux")]
const ROUTE_MONITOR_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub struct XdpConfig {
    pub interface: Option<String>,
    pub cpus: Vec<usize>,
    pub zero_copy: bool,
    // The capacity of the channel that sits between senders and each XDP thread that enqueues
    // packets to the NIC.
    pub tx_channel_cap: usize,
}

impl XdpConfig {
    // A nice round number
    const DEFAULT_TX_CHANNEL_CAP: usize = 1_000_000;
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            interface: None,
            cpus: vec![],
            zero_copy: false,
            tx_channel_cap: Self::DEFAULT_TX_CHANNEL_CAP,
        }
    }
}

impl XdpConfig {
    pub fn new(interface: Option<impl Into<String>>, cpus: Vec<usize>, zero_copy: bool) -> Self {
        Self {
            interface: interface.map(|s| s.into()),
            cpus,
            zero_copy,
            tx_channel_cap: XdpConfig::DEFAULT_TX_CHANNEL_CAP,
        }
    }
}

/// [`BytesTxPacket`] encapsulates the information needed to transmit a packet via XDP. Besides
/// the payload and destination addresses, it includes the source address of the packet.
#[cfg(target_os = "linux")]
pub struct BytesTxPacket {
    src_addr: SocketAddrV4,
    dst_addrs: XdpAddrs,
    ecn: Option<EcnCodepoint>,
    allow_mtu_overflow: bool,
    payload: Bytes,
}

#[cfg(not(target_os = "linux"))]
pub struct BytesTxPacket;

#[cfg(target_os = "linux")]
impl BytesTxPacket {
    pub fn new(
        src_addr: SocketAddrV4,
        dst_addrs: impl Into<XdpAddrs>,
        ecn: Option<EcnCodepoint>,
        payload: Bytes,
    ) -> Self {
        Self {
            src_addr,
            dst_addrs: dst_addrs.into(),
            ecn,
            allow_mtu_overflow: false,
            payload,
        }
    }

    /// Sets whether MTU overflow is possible for this packet.
    pub fn set_allow_mtu_overflow(&mut self, allow: bool) {
        self.allow_mtu_overflow = allow;
    }
}

#[cfg(not(target_os = "linux"))]
impl BytesTxPacket {
    pub fn new(
        _src_addr: SocketAddrV4,
        _dst_addrs: impl Into<XdpAddrs>,
        _ecn: Option<EcnCodepoint>,
        _payload: Bytes,
    ) -> Self {
        Self
    }

    pub fn set_allow_mtu_overflow(&mut self, _allow: bool) {}
}

#[cfg(target_os = "linux")]
impl TxPacket for BytesTxPacket {
    type Addrs = XdpAddrs;
    type Payload = Bytes;

    fn dst_addrs(&self) -> &Self::Addrs {
        &self.dst_addrs
    }

    fn payload(&self) -> &Self::Payload {
        &self.payload
    }

    fn src_addr(&self) -> SocketAddrV4 {
        self.src_addr
    }

    fn ecn(&self) -> Option<EcnCodepoint> {
        self.ecn
    }

    fn allow_mtu_overflow(&self) -> bool {
        self.allow_mtu_overflow
    }
}

#[derive(Clone)]
pub struct XdpSender {
    senders: Vec<Sender<BytesTxPacket>>,
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
    pub fn try_send(
        &self,
        sender_index: usize,
        packet: BytesTxPacket,
    ) -> Result<(), TrySendError<BytesTxPacket>> {
        let idx = sender_index
            .checked_rem(self.senders.len())
            .expect("XdpSender::senders should not be empty");
        self.senders[idx].try_send(packet)
    }

    pub fn len(&self) -> usize {
        self.senders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.senders.is_empty()
    }
}

pub struct Transmitter {
    threads: Vec<thread::JoinHandle<()>>,
}

#[cfg(not(target_os = "linux"))]
pub struct TransmitterBuilder {}

#[cfg(target_os = "linux")]
pub struct TransmitterBuilder {
    tx_loops: Vec<TxLoop<OwnedUmem<PageAlignedMemory>>>,
    tx_channel_cap: usize,
    maybe_ebpf: Option<Ebpf>,
    atomic_router: Arc<ArcSwap<Router>>,
    route_monitor_handle: thread::JoinHandle<()>,
}

impl TransmitterBuilder {
    #[cfg(not(target_os = "linux"))]
    pub fn new(_config: XdpConfig, _exit: Arc<AtomicBool>) -> Result<Self, Box<dyn Error>> {
        Err("XDP is only supported on Linux".into())
    }

    #[cfg(target_os = "linux")]
    pub fn new(config: XdpConfig, exit: Arc<AtomicBool>) -> Result<Self, Box<dyn Error>> {
        use {
            caps::Capability::{CAP_BPF, CAP_NET_ADMIN, CAP_NET_RAW, CAP_PERFMON},
            log::debug,
            std::{collections::HashSet, io},
        };
        let XdpConfig {
            interface: maybe_interface,
            cpus,
            zero_copy,
            tx_channel_cap,
        } = config;

        let dev = Arc::new(if let Some(interface) = maybe_interface {
            NetworkDevice::new(interface).unwrap()
        } else {
            NetworkDevice::new_from_default_route().unwrap()
        });

        let mut tx_loop_config_builder = TxLoopConfigBuilder::new();
        tx_loop_config_builder.zero_copy(zero_copy);
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
        let _setup_caps =
            CapGuard::raise([CAP_NET_ADMIN, CAP_NET_RAW]).expect("raise net capabilities");

        let maybe_ebpf_result = if zero_copy {
            let _ebpf_caps =
                CapGuard::raise([CAP_BPF, CAP_PERFMON]).expect("raise ebpf capabilities");

            let load_result =
                load_xdp_program(&dev).map_err(|e| format!("failed to attach xdp program: {e}"));

            Some(load_result)
        } else {
            None
        };

        let tx_loops = tx_loop_builders
            .into_iter()
            .map(|tx_loop_builder| tx_loop_builder.build())
            .collect::<Result<Vec<_>, io::Error>>()?;

        let tables_result = RoutingTables::from_netlink(RouteTable::Main);

        let tables = tables_result?;
        let router = Router::from_tables(tables)?;
        debug!(
            "published router table {}:\n{}",
            RouteTable::Main,
            router.routing_table()
        );

        // Use ArcSwap for lock-free updates of the routing table
        let atomic_router = Arc::new(ArcSwap::from_pointee(router));
        let route_monitor_handle = RouteMonitor::start(
            Arc::clone(&atomic_router),
            RouteTable::Main,
            exit.clone(),
            ROUTE_MONITOR_UPDATE_INTERVAL,
            || {
                // we need to retain CAP_NET_ADMIN in case the netlink socket needs reinitialized
                let retained_caps = caps::CapsHashSet::from_iter([caps::Capability::CAP_NET_ADMIN]);
                caps::set(None, caps::CapSet::Effective, &retained_caps)
                    .expect("linux allows effective capset to be set");
                caps::set(None, caps::CapSet::Permitted, &retained_caps)
                    .expect("linux allows permitted capset to be set");
                info!("route monitor thread started");
            },
        );

        let maybe_ebpf = maybe_ebpf_result.transpose()?;

        Ok(Self {
            tx_loops,
            tx_channel_cap,
            maybe_ebpf,
            atomic_router,
            route_monitor_handle,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn build(self) -> (Transmitter, XdpSender) {
        (
            Transmitter { threads: vec![] },
            XdpSender { senders: vec![] },
        )
    }

    #[cfg(target_os = "linux")]
    pub fn build(self) -> (Transmitter, XdpSender) {
        const DROP_CHANNEL_CAP: usize = 1_000_000;

        let Self {
            tx_loops,
            tx_channel_cap,
            maybe_ebpf,
            atomic_router,
            route_monitor_handle,
        } = self;

        let (drop_sender, drop_receiver) = crossbeam_channel::bounded(DROP_CHANNEL_CAP);
        let mut threads = vec![route_monitor_handle];

        threads.push(
            Builder::new()
                .name("solTransmDrop".to_owned())
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
            let (sender, receiver) = crossbeam_channel::bounded(tx_channel_cap);
            let drop_sender = drop_sender.clone();
            let atomic_router = Arc::clone(&atomic_router);
            threads.push(
                Builder::new()
                    .name(format!("solTransmIO{i:02}"))
                    .spawn(move || {
                        tx_loop.run(receiver, drop_sender, move |ip| {
                            let r = atomic_router.load();
                            match ip {
                                IpAddr::V4(ip) => r.route_v4(*ip).ok(),
                                IpAddr::V6(_) => None,
                            }
                        })
                    })
                    .unwrap(),
            );
            senders.push(sender);
        }

        (Transmitter { threads }, XdpSender { senders })
    }
}

impl Transmitter {
    pub fn join(self) -> thread::Result<()> {
        for handle in self.threads {
            handle.join()?;
        }
        Ok(())
    }
}

/// Returns the IPv4 address of the master interface if the given interface is part of a bond.
#[cfg(target_os = "linux")]
pub(crate) fn master_ip_if_bonded(interface: &str) -> Option<Ipv4Addr> {
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

#[cfg(target_os = "linux")]
const CAP_GUARD_CAPACITY: usize = 2;

#[cfg(target_os = "linux")]
#[must_use = "capabilities are dropped when the guard goes out of scope"]
struct CapGuard {
    capabilities: ArrayVec<caps::Capability, CAP_GUARD_CAPACITY>,
}

#[cfg(target_os = "linux")]
impl CapGuard {
    fn raise(
        raised_capabilities: impl IntoIterator<Item = caps::Capability>,
    ) -> Result<Self, caps::errors::CapsError> {
        let mut capabilities = ArrayVec::new();
        for capability in raised_capabilities {
            capabilities.try_push(capability).unwrap_or_else(|_| {
                panic!("CapGuard supports at most {CAP_GUARD_CAPACITY} capabilities")
            });
            caps::raise(None, caps::CapSet::Effective, capability)?;
        }
        Ok(Self { capabilities })
    }
}

#[cfg(target_os = "linux")]
impl Drop for CapGuard {
    fn drop(&mut self) {
        for capability in self.capabilities.iter().rev() {
            caps::drop(None, caps::CapSet::Effective, *capability)
                .unwrap_or_else(|err| panic!("drop {capability:?} capability: {err}"));
        }
    }
}
