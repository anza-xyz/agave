use {
    crate::ecn_codepoint::EcnCodepoint,
    bytes::Bytes,
    std::{
        error::Error,
        net::{SocketAddr, SocketAddrV4},
        sync::{Arc, atomic::AtomicBool},
        thread,
        time::Duration,
    },
};
#[cfg(target_os = "linux")]
use {
    crate::{
        device::{NetworkDevice, QueueId},
        load_xdp_program,
        netlink::MacAddress,
        route::{RouteTable, Router, RoutingTables},
        route_monitor::RouteMonitor,
        tx_loop::{self, PacketBuildParams, TxLoop, TxLoopBuilder, TxSender, build_packet},
        umem::{Frame, OwnedUmem, OwnedUmemFrame, Umem},
    },
    agave_cpu_utils::{CpuId, cpu_affinity, set_cpu_affinity},
    arc_swap::ArcSwap,
    arrayvec::ArrayVec,
    aya::Ebpf,
    log::info,
    std::{net::Ipv4Addr, thread::Builder},
};

#[cfg(target_os = "linux")]
const ROUTE_MONITOR_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

/// Binding of a single NIC hardware TX queue to a CPU core.
///
/// Each binding becomes one TX worker thread, pinned to `cpu`, whose AF_XDP
/// socket is bound to hardware queue `queue` on the configured interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueCpuBinding {
    /// NIC hardware TX queue id the AF_XDP socket binds to.
    pub queue: u32,
    /// Logical CPU core the worker thread is pinned to.
    pub cpu: usize,
}

#[derive(Clone, Debug, Default)]
pub struct XdpConfig {
    pub interface: Option<String>,
    /// NIC-queue -> CPU-core bindings. One TX worker is created per entry, in
    /// order. The queue id is taken explicitly from the binding rather than
    /// inferred from position, so callers can target arbitrary hardware queues.
    pub queues: Vec<QueueCpuBinding>,
    pub zero_copy: bool,
}

impl XdpConfig {
    pub fn new(
        interface: Option<impl Into<String>>,
        queues: Vec<QueueCpuBinding>,
        zero_copy: bool,
    ) -> Self {
        Self {
            interface: interface.map(|s| s.into()),
            queues,
            zero_copy,
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
#[derive(Debug)]
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
impl std::fmt::Debug for BytesTxPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BytesTxPacket")
            .field("src_addr", &self.src_addr)
            .field("num_destinations", &self.dst_addrs.as_ref().len())
            .field("ecn", &self.ecn)
            .field("allow_mtu_overflow", &self.allow_mtu_overflow)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

/// Error returned when an XDP packet cannot be fully enqueued.
///
/// The second field is the number of unsent destinations.
#[derive(Debug)]
pub enum TrySendError<T> {
    Full(T, usize),
    Disconnected(T, usize),
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub struct XdpSender {
    queues: Vec<TxQueue>,
    atomic_router: Arc<ArcSwap<Router>>,
    src_mac: MacAddress,
}

#[cfg(not(target_os = "linux"))]
#[derive(Clone)]
pub struct XdpSender;

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct TxQueue {
    sender: TxSender<OwnedUmemFrame>,
    umem: Arc<OwnedUmem>,
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
    /// Attempts to send a `packet` without blocking.
    #[inline]
    pub fn try_send(
        &self,
        sender_index: usize,
        packet: BytesTxPacket,
    ) -> Result<(), TrySendError<BytesTxPacket>> {
        #[cfg(target_os = "linux")]
        {
            let idx = sender_index
                .checked_rem(self.queues.len())
                .expect("XdpSender::queues should not be empty");
            let queue = &self.queues[idx];
            let router = self.atomic_router.load();

            for (i, addr) in packet.dst_addrs.as_ref().iter().enumerate() {
                let addr = *addr;
                match self.try_send_one(queue, &router, &packet, addr) {
                    Ok(_) => {}
                    Err(tx_loop::TrySendError::Full(())) => {
                        let num_unsent = packet.dst_addrs.as_ref().len().saturating_sub(i);
                        return Err(TrySendError::Full(packet, num_unsent));
                    }
                    Err(tx_loop::TrySendError::Disconnected(())) => {
                        let num_unsent = packet.dst_addrs.as_ref().len().saturating_sub(i);
                        return Err(TrySendError::Disconnected(packet, num_unsent));
                    }
                };
            }

            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = sender_index;
            Err(TrySendError::Disconnected(packet, 0))
        }
    }

    #[cfg(target_os = "linux")]
    fn try_send_one(
        &self,
        queue: &TxQueue,
        router: &Router,
        packet: &BytesTxPacket,
        addr: SocketAddr,
    ) -> Result<(), tx_loop::TrySendError<()>> {
        let SocketAddr::V4(dst_addr) = addr else {
            panic!("IPv6 not supported");
        };
        let Ok(next_hop) = router.route_v4(*dst_addr.ip()) else {
            log::warn!("dropping packet: no route for peer {addr}");
            return Ok(());
        };

        let Some(mut frame) = queue.umem.reserve() else {
            return Err(tx_loop::TrySendError::Full(()));
        };
        frame.set_len(queue.umem.frame_size());

        let mut mapped = queue.umem.map_frame_mut(frame);
        let Some(packet_len) = build_packet(PacketBuildParams {
            packet: &mut mapped,
            src_mac: &self.src_mac,
            src_addr: packet.src_addr,
            dst_addr,
            next_hop: &next_hop,
            payload: packet.payload.as_ref(),
            ecn: packet.ecn,
            allow_mtu_overflow: packet.allow_mtu_overflow,
        }) else {
            queue.umem.release(mapped.into_frame());
            return Ok(());
        };

        let mut frame = mapped.into_frame();
        frame.set_len(packet_len);

        if let Err(err) = queue.sender.try_send(frame) {
            match err {
                tx_loop::TrySendError::Full(frame) => {
                    queue.umem.release(frame);
                    Err(tx_loop::TrySendError::Full(()))
                }
                tx_loop::TrySendError::Disconnected(frame) => {
                    queue.umem.release(frame);
                    Err(tx_loop::TrySendError::Disconnected(()))
                }
            }
        } else {
            Ok(())
        }
    }

    pub fn len(&self) -> usize {
        #[cfg(target_os = "linux")]
        return self.queues.len();

        #[cfg(not(target_os = "linux"))]
        0
    }

    pub fn is_empty(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.queues.is_empty();

        #[cfg(not(target_os = "linux"))]
        true
    }
}

pub struct Transmitter {
    threads: Vec<thread::JoinHandle<()>>,
    #[cfg(target_os = "linux")]
    _maybe_ebpf: Option<Ebpf>,
}

#[cfg(not(target_os = "linux"))]
pub struct TransmitterBuilder {}

#[cfg(target_os = "linux")]
pub struct TransmitterBuilder {
    tx_loops: Vec<TxLoop<Arc<OwnedUmem>>>,
    src_mac: MacAddress,
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
            queues,
            zero_copy,
        } = config;

        let dev = Arc::new(if let Some(interface) = maybe_interface {
            NetworkDevice::new(interface).unwrap()
        } else {
            NetworkDevice::new_from_default_route().unwrap()
        });

        let reserved_cores = queues
            .iter()
            .map(|binding| CpuId::new(binding.cpu))
            .collect::<io::Result<HashSet<_>>>()?;
        let unreserved_cores = cpu_affinity(None)?
            .into_iter()
            .filter(|core| !reserved_cores.contains(core))
            .collect::<Vec<_>>();

        if unreserved_cores.is_empty() {
            return Err("all CPUs are reserved; no CPU available for the main thread".into());
        }

        let mut tx_loop_builders = Vec::with_capacity(queues.len());
        for binding in queues {
            // since we aren't necessarily allocating from the thread that we intend to run on,
            // temporarily switch to the target cpu for each TxLoop to ensure that the Umem region
            // is allocated to the correct numa node
            let cpu = CpuId::new(binding.cpu)?;
            set_cpu_affinity(None, [cpu])?;
            let tx_loop_builder =
                TxLoopBuilder::new(binding.cpu, QueueId(binding.queue as u64), zero_copy, &dev);
            // migrate main thread back off of the last xdp reserved cpu
            set_cpu_affinity(None, unreserved_cores.iter().copied())?;
            tx_loop_builders.push(tx_loop_builder);
        }

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
            src_mac: dev.mac_addr()?,
            maybe_ebpf,
            atomic_router,
            route_monitor_handle,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn build(self) -> (Transmitter, XdpSender) {
        (Transmitter { threads: vec![] }, XdpSender {})
    }

    #[cfg(target_os = "linux")]
    pub fn build(self) -> (Transmitter, XdpSender) {
        let Self {
            tx_loops,
            src_mac,
            maybe_ebpf,
            atomic_router,
            route_monitor_handle,
        } = self;

        let mut threads = vec![route_monitor_handle];

        let mut queues = vec![];
        for (i, tx_loop) in tx_loops.into_iter().enumerate() {
            let umem = Arc::clone(tx_loop.umem());
            let (sender, receiver) = tx_loop::channel(tx_loop.umem_tx_capacity());
            threads.push(
                Builder::new()
                    .name(format!("solTransmIO{i:02}"))
                    .spawn(move || tx_loop.run(receiver))
                    .unwrap(),
            );
            queues.push(TxQueue { sender, umem });
        }

        (
            Transmitter {
                threads,
                _maybe_ebpf: maybe_ebpf,
            },
            XdpSender {
                queues,
                atomic_router,
                src_mac,
            },
        )
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
