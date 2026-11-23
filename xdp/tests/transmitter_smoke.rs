#![cfg(target_os = "linux")]

mod common;

use {
    agave_cpu_utils::cpu_affinity,
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        gre::packet::GRE_HEADER_BASE_SIZE,
        netlink::{MacAddress, netlink_get_neighbors},
        packet::{ETH_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE},
        route::NextHop,
        transmitter::{
            BytesTxPacket, NeighborIntervals, QueueCpuBinding, Transmitter, TransmitterBuilder,
            XdpConfig, XdpSender,
        },
        tx_loop::{self, TrySendError, TxLoopBuilder, TxLoopConfigBuilder, TxPacket, TxReceiver},
    },
    bytes::Bytes,
    nix::{
        errno::Errno,
        poll::{PollFd, PollFlags, PollTimeout, poll},
        sys::socket::{
            AddressFamily, MsgFlags, SockFlag, SockProtocol, SockType, SockaddrLike,
            SockaddrStorage, bind, recv, setsockopt, socket, sockopt::RcvBufForce,
        },
    },
    std::{
        fs, io, mem,
        net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
        ops::Range,
        os::fd::{AsFd, AsRawFd, OwnedFd},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

const TEST_TX_CHANNEL_CAP: usize = 16;

fn transmitter_cpus<const COUNT: usize>() -> [usize; COUNT] {
    let cores = cpu_affinity(None).expect("linux provides affine cores");
    assert!(
        cores.len() > COUNT,
        "transmitter smoke test requires at least {} affine CPU cores, found {}",
        COUNT.saturating_add(1),
        cores.len(),
    );
    std::array::from_fn(|index| *cores[index])
}

fn transmitter_cpu() -> usize {
    let [cpu_id] = transmitter_cpus();
    cpu_id
}

struct PacketSocket {
    fd: OwnedFd,
}

struct TransmitterGuard {
    transmitter: Option<Transmitter>,
    sender: Option<XdpSender>,
    exit: Arc<AtomicBool>,
}

impl TransmitterGuard {
    fn new(transmitter: Transmitter, sender: XdpSender, exit: Arc<AtomicBool>) -> Self {
        Self {
            transmitter: Some(transmitter),
            sender: Some(sender),
            exit,
        }
    }

    fn sender(&self) -> &XdpSender {
        self.sender.as_ref().expect("sender is live")
    }
}

impl Drop for TransmitterGuard {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        drop(self.sender.take());
        let Some(transmitter) = self.transmitter.take() else {
            return;
        };
        if let Err(err) = transmitter.join() {
            if std::thread::panicking() {
                eprintln!("transmitter thread panicked: {err:?}");
            } else {
                std::panic::resume_unwind(err);
            }
        }
    }
}

impl PacketSocket {
    fn bind(if_index: u32) -> io::Result<Self> {
        let fd = socket(
            AddressFamily::Packet,
            SockType::Raw,
            SockFlag::SOCK_CLOEXEC,
            SockProtocol::EthAll,
        )
        .map_err(io::Error::from)?;
        let addr = libc::sockaddr_ll {
            sll_family: libc::AF_PACKET as u16,
            sll_protocol: (libc::ETH_P_ALL as u16).to_be(),
            sll_ifindex: if_index as i32,
            sll_hatype: 0,
            sll_pkttype: 0,
            sll_halen: 0,
            sll_addr: [0; 8],
        };
        let addr = unsafe {
            SockaddrStorage::from_raw(
                (&addr as *const libc::sockaddr_ll).cast(),
                Some(mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t),
            )
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid packet address"))?;
        bind(fd.as_raw_fd(), &addr).map_err(io::Error::from)?;
        Ok(Self { fd })
    }

    /// Makes room for frames that arrive before the test reads them.
    fn set_receive_buffer(&self, bytes: usize) -> io::Result<()> {
        setsockopt(&self.fd, RcvBufForce, &bytes).map_err(io::Error::from)
    }

    /// Counts the frames already received that match `expected`, without waiting for more.
    fn count_received_udp(&self, expected: &ExpectedUdpPacket<'_>) -> usize {
        let mut buf = [0u8; 2048];
        let mut count = 0usize;
        loop {
            match recv(self.fd.as_raw_fd(), &mut buf, MsgFlags::MSG_DONTWAIT) {
                Ok(len) => {
                    if matching_udp_payload(&buf[..len], expected).is_some() {
                        count = count.saturating_add(1);
                    }
                }
                Err(Errno::EINTR) => {}
                Err(_) => return count,
            }
        }
    }

    fn recv_matching_udp<'a>(
        &self,
        buf: &'a mut [u8],
        expected: &ExpectedUdpPacket<'_>,
        timeout: Duration,
    ) -> io::Result<&'a [u8]> {
        self.recv_matching_payload("matching UDP frame", buf, timeout, |frame| {
            matching_udp_payload(frame, expected)
        })
    }

    fn recv_matching_gre_udp<'a>(
        &self,
        buf: &'a mut [u8],
        expected: &ExpectedGreUdpPacket<'_>,
        timeout: Duration,
    ) -> io::Result<&'a [u8]> {
        self.recv_matching_payload("matching GRE UDP frame", buf, timeout, |frame| {
            matching_gre_udp_payload(frame, expected)
        })
    }

    fn recv_matching_payload<'a, F>(
        &self,
        description: &str,
        buf: &'a mut [u8],
        timeout: Duration,
        mut matcher: F,
    ) -> io::Result<&'a [u8]>
    where
        F: FnMut(&[u8]) -> Option<Range<usize>>,
    {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "timeout overflows instant")
        })?;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for {description}"),
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let mut pfd = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
            let timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
            match poll(&mut pfd, timeout) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(err) => return Err(io::Error::from(err)),
            }

            let len = match recv(self.fd.as_raw_fd(), buf, MsgFlags::empty()) {
                Ok(len) => len,
                Err(Errno::EINTR) => continue,
                Err(err) => return Err(io::Error::from(err)),
            };
            let frame = &buf[..len];
            if let Some(payload_range) = matcher(frame) {
                return Ok(&buf[payload_range]);
            }
        }
    }
}

struct ExpectedUdpPacket<'a> {
    src_mac: MacAddress,
    dst_mac: MacAddress,
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &'a [u8],
}

struct ExpectedGreUdpPacket<'a> {
    outer_src_mac: MacAddress,
    outer_dst_mac: MacAddress,
    outer_src_ip: Ipv4Addr,
    outer_dst_ip: Ipv4Addr,
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &'a [u8],
}

struct ExpectedUdpDatagram<'a> {
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &'a [u8],
}

fn matching_udp_payload(frame: &[u8], expected: &ExpectedUdpPacket<'_>) -> Option<Range<usize>> {
    if frame.len() < ETH_HEADER_SIZE {
        return None;
    }
    if frame[0..6] != expected.dst_mac.0 || frame[6..12] != expected.src_mac.0 {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != libc::ETH_P_IP as u16 {
        return None;
    }

    let payload_range = matching_ipv4_udp_payload(
        &frame[ETH_HEADER_SIZE..],
        &ExpectedUdpDatagram {
            src_ip: expected.src_ip,
            dst_ip: expected.dst_ip,
            src_port: expected.src_port,
            dst_port: expected.dst_port,
            payload: expected.payload,
        },
    )?;
    Some(
        payload_range.start.checked_add(ETH_HEADER_SIZE).unwrap()
            ..payload_range.end.checked_add(ETH_HEADER_SIZE).unwrap(),
    )
}

fn matching_gre_udp_payload(
    frame: &[u8],
    expected: &ExpectedGreUdpPacket<'_>,
) -> Option<Range<usize>> {
    const GRE_FLAGS_VERSION_BASIC: u16 = 0x0000;

    if frame.len() < ETH_HEADER_SIZE.checked_add(IP_HEADER_SIZE)? {
        return None;
    }
    if frame[0..6] != expected.outer_dst_mac.0 || frame[6..12] != expected.outer_src_mac.0 {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != libc::ETH_P_IP as u16 {
        return None;
    }

    let outer_ip = &frame[ETH_HEADER_SIZE..];
    let outer_ihl = usize::from(outer_ip[0] & 0x0f).checked_mul(4)?;
    let gre_offset = ETH_HEADER_SIZE.checked_add(outer_ihl)?;
    let min_frame_len = gre_offset
        .checked_add(GRE_HEADER_BASE_SIZE)?
        .checked_add(IP_HEADER_SIZE)?;
    if outer_ihl < IP_HEADER_SIZE || frame.len() < min_frame_len {
        return None;
    }
    if outer_ip[9] != libc::IPPROTO_GRE as u8 {
        return None;
    }
    if outer_ip[12..16] != expected.outer_src_ip.octets()
        || outer_ip[16..20] != expected.outer_dst_ip.octets()
    {
        return None;
    }

    let gre = &frame[gre_offset..];
    if u16::from_be_bytes([gre[0], gre[1]]) != GRE_FLAGS_VERSION_BASIC {
        return None;
    }
    if u16::from_be_bytes([gre[2], gre[3]]) != libc::ETH_P_IP as u16 {
        return None;
    }

    let inner_offset = gre_offset.checked_add(GRE_HEADER_BASE_SIZE)?;
    let payload_range = matching_ipv4_udp_payload(
        frame.get(inner_offset..)?,
        &ExpectedUdpDatagram {
            src_ip: expected.inner_src_ip,
            dst_ip: expected.inner_dst_ip,
            src_port: expected.src_port,
            dst_port: expected.dst_port,
            payload: expected.payload,
        },
    )?;
    Some(
        payload_range.start.checked_add(inner_offset).unwrap()
            ..payload_range.end.checked_add(inner_offset).unwrap(),
    )
}

fn matching_ipv4_udp_payload(
    ip: &[u8],
    expected: &ExpectedUdpDatagram<'_>,
) -> Option<Range<usize>> {
    let min_udp_len = IP_HEADER_SIZE.checked_add(UDP_HEADER_SIZE)?;
    if ip.len() < min_udp_len {
        return None;
    }

    let ihl = usize::from(ip[0] & 0x0f).checked_mul(4)?;
    let min_packet_len = ihl.checked_add(UDP_HEADER_SIZE)?;
    if ihl < IP_HEADER_SIZE || ip.len() < min_packet_len {
        return None;
    }
    if ip[9] != libc::IPPROTO_UDP as u8 {
        return None;
    }
    if ip[12..16] != expected.src_ip.octets() || ip[16..20] != expected.dst_ip.octets() {
        return None;
    }

    let udp = &ip[ihl..];
    if u16::from_be_bytes([udp[0], udp[1]]) != expected.src_port
        || u16::from_be_bytes([udp[2], udp[3]]) != expected.dst_port
    {
        return None;
    }
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp_len < UDP_HEADER_SIZE || udp.len() < udp_len {
        return None;
    }

    let payload_start = ihl.checked_add(UDP_HEADER_SIZE).unwrap();
    let payload_end = ihl.checked_add(udp_len).unwrap();
    let payload = &ip[payload_start..payload_end];
    (payload == expected.payload).then_some(payload_start..payload_end)
}

/// Queues one packet with `send` on a copy-mode transmitter on the left veth end and expects its
/// frame on the right end.
fn assert_transmitter_sends_udp_payload_over_veth(
    send: impl FnOnce(&XdpSender, BytesTxPacket) -> Result<(), TrySendError<BytesTxPacket>>,
) {
    let cpu_id = transmitter_cpu();

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);
    common::replace_neighbor(links.right_ip, links.right_mac, common::LEFT_IFACE);

    let receiver = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let dst_port = 45_678;
    let src_port = 12_345;
    let destination = SocketAddr::V4(SocketAddrV4::new(links.right_ip, dst_port));
    let payload = Bytes::from_static(b"agave-xdp-transmitter-smoke");

    let exit = Arc::new(AtomicBool::new(false));
    let config = XdpConfig::with_tx_channel_cap(
        Some(common::LEFT_IFACE.to_string()),
        vec![QueueCpuBinding {
            queue: 0,
            cpu: cpu_id,
        }],
        false,
        TEST_TX_CHANNEL_CAP,
    );

    let (transmitter, sender) = TransmitterBuilder::new(config, Arc::clone(&exit))
        .expect("build copy-mode transmitter")
        .build();
    let transmitter = TransmitterGuard::new(transmitter, sender, exit);

    let packet = BytesTxPacket::new(
        SocketAddrV4::new(links.left_ip, src_port),
        destination,
        None,
        payload.clone(),
    );
    send(transmitter.sender(), packet).expect("queue packet through XdpSender");

    let mut buf = [0u8; 2048];
    let received = receiver
        .recv_matching_udp(
            &mut buf,
            &ExpectedUdpPacket {
                src_mac: links.left_mac,
                dst_mac: links.right_mac,
                src_ip: links.left_ip,
                dst_ip: links.right_ip,
                src_port,
                dst_port,
                payload: payload.as_ref(),
            },
            Duration::from_secs(3),
        )
        .expect("receive UDP frame from AF_XDP transmitter");
    assert_eq!(received, payload.as_ref());
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn transmitter_sends_udp_payload_over_veth_in_copy_mode() {
    assert_transmitter_sends_udp_payload_over_veth(|sender, packet| sender.try_send(0, packet));
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn transmitter_sends_low_priority_udp_payload_over_veth_in_copy_mode() {
    assert_transmitter_sends_udp_payload_over_veth(|sender, packet| {
        sender.try_send_low_priority(0, packet)
    });
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn transmitter_resolves_neighbors() {
    let cpu_id = transmitter_cpu();

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);

    // both veth endpoints are local, configure the peer to accept ARP from a local source address.
    fs::write("/proc/sys/net/ipv4/conf/axdp1/accept_local", "1")
        .expect("enable accept_local on veth peer");

    let neighbor_intervals = NeighborIntervals {
        use_interval: Duration::from_millis(100),
        miss_interval: Duration::from_millis(10),
    };
    // how long before a neighbor entry is considered stale and needs to be touched again
    fs::write(
        "/proc/sys/net/ipv4/neigh/axdp0/base_reachable_time_ms",
        "500",
    )
    .expect("shorten neighbor reachable time");
    fs::write("/proc/sys/net/ipv4/neigh/axdp0/delay_first_probe_time", "0")
        .expect("disable neighbor probe delay");

    let receiver = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let dst_port = 45_682;
    let src_port = 12_348;
    let destination = SocketAddr::V4(SocketAddrV4::new(links.right_ip, dst_port));

    let exit = Arc::new(AtomicBool::new(false));
    let config = XdpConfig::with_tx_channel_cap(
        Some(common::LEFT_IFACE.to_string()),
        vec![QueueCpuBinding {
            queue: 0,
            cpu: cpu_id,
        }],
        false,
        TEST_TX_CHANNEL_CAP,
    );
    let (transmitter, sender) =
        TransmitterBuilder::new_with_intervals(config, Arc::clone(&exit), neighbor_intervals)
            .expect("build copy-mode transmitter")
            .build();
    let transmitter = TransmitterGuard::new(transmitter, sender, exit);

    let neighbors = netlink_get_neighbors(None, libc::AF_INET as u8).expect("read neighbor table");
    assert!(
        neighbors.iter().all(|neighbor| {
            neighbor.ifindex != links.left_if_index as i32
                || neighbor.destination != Some(IpAddr::V4(links.right_ip))
        }),
        "neighbor unexpectedly exists before sending"
    );

    // this should resolve the neighbor. The packet is lost because the neighbor is not yet
    // reachable.
    let packet = BytesTxPacket::new(
        SocketAddrV4::new(links.left_ip, src_port),
        destination,
        None,
        Bytes::from_static(b"agave-xdp-neighbor-resolution"),
    );
    transmitter
        .sender()
        .try_send(0, packet)
        .expect("queue packet to resolve neighbor");

    common::wait_until(
        "neighbor to become reachable",
        Duration::from_secs(3),
        || {
            netlink_get_neighbors(None, libc::AF_INET as u8)
                .expect("read neighbor table")
                .into_iter()
                .find(|neighbor| {
                    neighbor.ifindex == links.left_if_index as i32
                        && neighbor.destination == Some(IpAddr::V4(links.right_ip))
                        && neighbor.lladdr == Some(links.right_mac)
                        && neighbor.state & libc::NUD_REACHABLE != 0
                })
        },
    );

    // now wait for it to go stale
    common::wait_until("neighbor to become stale", Duration::from_secs(3), || {
        netlink_get_neighbors(None, libc::AF_INET as u8)
            .expect("read neighbor table")
            .into_iter()
            .find(|neighbor| {
                neighbor.ifindex == links.left_if_index as i32
                    && neighbor.destination == Some(IpAddr::V4(links.right_ip))
                    && neighbor.lladdr == Some(links.right_mac)
                    && neighbor.state & libc::NUD_STALE != 0
            })
    });

    // this should refresh
    let payload = Bytes::from_static(b"agave-xdp-neighbor-touch");
    let packet = BytesTxPacket::new(
        SocketAddrV4::new(links.left_ip, src_port),
        destination,
        None,
        payload.clone(),
    );
    transmitter
        .sender()
        .try_send(0, packet)
        .expect("queue packet to touch stale neighbor");

    common::wait_until(
        "stale neighbor to be touched",
        Duration::from_secs(3),
        || {
            netlink_get_neighbors(None, libc::AF_INET as u8)
                .expect("read neighbor table")
                .into_iter()
                .find(|neighbor| {
                    let touched = libc::NUD_DELAY | libc::NUD_PROBE | libc::NUD_REACHABLE;
                    neighbor.ifindex == links.left_if_index as i32
                        && neighbor.destination == Some(IpAddr::V4(links.right_ip))
                        && neighbor.lladdr == Some(links.right_mac)
                        && neighbor.state & touched != 0
                })
        },
    );

    let mut buf = [0u8; 2048];
    let received = receiver
        .recv_matching_udp(
            &mut buf,
            &ExpectedUdpPacket {
                src_mac: links.left_mac,
                dst_mac: links.right_mac,
                src_ip: links.left_ip,
                dst_ip: links.right_ip,
                src_port,
                dst_port,
                payload: payload.as_ref(),
            },
            Duration::from_secs(3),
        )
        .expect("receive UDP frame after touching stale neighbor");
    assert_eq!(received, payload.as_ref());
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn transmitter_sends_udp_payload_over_two_queues_in_copy_mode() {
    let [first_cpu, second_cpu] = transmitter_cpus();

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(2);
    common::replace_neighbor(links.right_ip, links.right_mac, common::LEFT_IFACE);

    let receiver = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let src_port = 12_347;

    let exit = Arc::new(AtomicBool::new(false));
    let config = XdpConfig::with_tx_channel_cap(
        Some(common::LEFT_IFACE.to_string()),
        vec![
            QueueCpuBinding {
                queue: 0,
                cpu: first_cpu,
            },
            QueueCpuBinding {
                queue: 1,
                cpu: second_cpu,
            },
        ],
        false,
        TEST_TX_CHANNEL_CAP,
    );

    let (transmitter, sender) = TransmitterBuilder::new(config, Arc::clone(&exit))
        .expect("build two-queue copy-mode transmitter")
        .build();
    let transmitter = TransmitterGuard::new(transmitter, sender, exit);
    assert_eq!(transmitter.sender().len(), 2);

    let mut buf = [0u8; 2048];
    for (sender_index, dst_port, payload) in [
        (
            0,
            45_680,
            Bytes::from_static(b"agave-xdp-transmitter-queue-0"),
        ),
        (
            1,
            45_681,
            Bytes::from_static(b"agave-xdp-transmitter-queue-1"),
        ),
    ] {
        let destination = SocketAddr::V4(SocketAddrV4::new(links.right_ip, dst_port));
        let packet = BytesTxPacket::new(
            SocketAddrV4::new(links.left_ip, src_port),
            destination,
            None,
            payload.clone(),
        );
        transmitter
            .sender()
            .try_send(sender_index, packet)
            .unwrap_or_else(|err| panic!("queue packet through sender {sender_index}: {err}"));

        let received = receiver
            .recv_matching_udp(
                &mut buf,
                &ExpectedUdpPacket {
                    src_mac: links.left_mac,
                    dst_mac: links.right_mac,
                    src_ip: links.left_ip,
                    dst_ip: links.right_ip,
                    src_port,
                    dst_port,
                    payload: payload.as_ref(),
                },
                Duration::from_secs(3),
            )
            .unwrap_or_else(|err| panic!("receive UDP frame from sender {sender_index}: {err}"));
        assert_eq!(received, payload.as_ref());
    }
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn transmitter_sends_udp_payload_over_gre_tunnel_in_copy_mode() {
    let cpu_id = transmitter_cpu();

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);
    common::replace_neighbor(links.right_ip, links.right_mac, common::LEFT_IFACE);
    common::add_route_to_dev(&format!("{}/32", links.right_ip), common::LEFT_IFACE);
    let gre = common::setup_gre_tunnel(&links);
    common::add_route_to_dev_with_src("192.0.2.0/24", common::GRE_IFACE, gre.overlay_ip);

    // Sending to the overlay destination exercises route lookup plus GRE encapsulation.
    // The raw receiver observes the outer packet on the underlay veth peer.
    let receiver = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let dst_port = 45_679;
    let src_port = 12_346;
    let overlay_destination = Ipv4Addr::new(192, 0, 2, 99);
    let destination = SocketAddr::V4(SocketAddrV4::new(overlay_destination, dst_port));
    let payload = Bytes::from_static(b"agave-xdp-transmitter-gre-smoke");

    let exit = Arc::new(AtomicBool::new(false));
    let config = XdpConfig::with_tx_channel_cap(
        Some(common::LEFT_IFACE.to_string()),
        vec![QueueCpuBinding {
            queue: 0,
            cpu: cpu_id,
        }],
        false,
        TEST_TX_CHANNEL_CAP,
    );

    let (transmitter, sender) = TransmitterBuilder::new(config, Arc::clone(&exit))
        .expect("build copy-mode transmitter")
        .build();
    let transmitter = TransmitterGuard::new(transmitter, sender, exit);

    let packet = BytesTxPacket::new(
        SocketAddrV4::new(links.left_ip, src_port),
        destination,
        None,
        payload.clone(),
    );
    transmitter
        .sender()
        .try_send(0, packet)
        .expect("queue packet through XdpSender::try_send");

    let mut buf = [0u8; 2048];
    let received = receiver
        .recv_matching_gre_udp(
            &mut buf,
            &ExpectedGreUdpPacket {
                outer_src_mac: links.left_mac,
                outer_dst_mac: links.right_mac,
                outer_src_ip: gre.local_ip,
                outer_dst_ip: gre.remote_ip,
                inner_src_ip: gre.overlay_ip,
                inner_dst_ip: overlay_destination,
                src_port,
                dst_port,
                payload: payload.as_ref(),
            },
            Duration::from_secs(3),
        )
        .expect("receive GRE-encapsulated UDP frame from AF_XDP transmitter");
    assert_eq!(received, payload.as_ref());
}

const TX_LOOP_SRC_PORT: u16 = 12_348;
const TX_LOOP_DST_PORT: u16 = 45_690;

/// The frame a packet queued by [`tx_loop_packet`] becomes on the right veth end.
fn tx_loop_frame<'a>(links: &common::TestLinks, payload: &'a [u8]) -> ExpectedUdpPacket<'a> {
    ExpectedUdpPacket {
        src_mac: links.left_mac,
        dst_mac: links.right_mac,
        src_ip: links.left_ip,
        dst_ip: links.right_ip,
        src_port: TX_LOOP_SRC_PORT,
        dst_port: TX_LOOP_DST_PORT,
        payload,
    }
}

fn tx_loop_packet(links: &common::TestLinks, dst_ip: Ipv4Addr, payload: Bytes) -> BytesTxPacket {
    BytesTxPacket::new(
        SocketAddrV4::new(links.left_ip, TX_LOOP_SRC_PORT),
        SocketAddr::V4(SocketAddrV4::new(dst_ip, TX_LOOP_DST_PORT)),
        None,
        payload,
    )
}

/// Runs a copy-mode tx loop on the left veth end until every sender of `receiver` is gone. The
/// test owns the channel and the `drop_item` callback, which the loop calls after each item, so
/// it can act at an exact point of the loop. Only the right veth end is routable.
fn run_tx_loop(
    links: &common::TestLinks,
    receiver: TxReceiver<BytesTxPacket>,
    drop_item: impl FnMut(BytesTxPacket),
) {
    let dev = NetworkDevice::new(common::LEFT_IFACE).expect("open the left veth end");
    let config = TxLoopConfigBuilder::new()
        .build_with_src_device(&dev)
        .expect("read the MAC address of the left veth end");
    let tx_loop = TxLoopBuilder::new(transmitter_cpu(), QueueId(0), config, &dev)
        .expect("allocate the tx loop")
        .build()
        .expect("build a copy-mode tx loop");
    let next_hop = NextHop {
        mac_addr: Some(links.right_mac),
        ip_addr: IpAddr::V4(links.right_ip),
        if_index: links.left_if_index,
        mtu: 1500,
        neigh_requires_refresh: false,
        preferred_src_ip: None,
        gre: None,
        vlan: None,
    };
    let routable = IpAddr::V4(links.right_ip);
    tx_loop.run(receiver, drop_item, |ip| {
        (*ip == routable).then(|| next_hop.clone())
    });
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn tx_loop_sends_low_priority_packets_when_nothing_else_waits() {
    const LOW_PRIORITY_PACKETS: usize = 200;
    const PACKETS: usize = 64;
    // The other packets are queued right after the loop is done with this low priority packet.
    const LAST_LOW_PRIORITY_PACKET_BEFORE_THE_OTHERS: usize = 9;

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);
    let capture = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    capture
        .set_receive_buffer(8 << 20)
        .expect("grow the receive buffer of the raw packet receiver");

    let low_priority_payload = |i: usize| Bytes::from(format!("low-priority-{i:03}"));
    let payload = |i: usize| Bytes::from(format!("packet-{i:03}"));

    let (sender, receiver) = tx_loop::channel(PACKETS, LOW_PRIORITY_PACKETS);
    for i in 0..LOW_PRIORITY_PACKETS {
        sender
            .try_send_low_priority(tx_loop_packet(
                &links,
                links.right_ip,
                low_priority_payload(i),
            ))
            .expect("queue low priority packet");
    }

    // The loop queues the other packets itself, with this sender. The test holds it so that it
    // can end the loop even when the loop never gets to that point.
    let loop_sender = Mutex::new(Some(sender.clone()));
    let mismatch = std::thread::scope(|scope| {
        let (links, loop_sender) = (&links, &loop_sender);
        let trigger = low_priority_payload(LAST_LOW_PRIORITY_PACKET_BEFORE_THE_OTHERS);
        scope.spawn(move || {
            run_tx_loop(links, receiver, |item| {
                if item.payload() == &trigger {
                    let sender = loop_sender.lock().unwrap().take();
                    let sender = sender.expect("the trigger is sent once");
                    for i in 0..PACKETS {
                        sender
                            .try_send(tx_loop_packet(links, links.right_ip, payload(i)))
                            .expect("queue packet");
                    }
                }
            });
        });

        // The loop sends everything that is not low priority first, however many low priority
        // packets wait, and then goes on with those.
        let first_low_priority = 0..=LAST_LOW_PRIORITY_PACKET_BEFORE_THE_OTHERS;
        let rest_low_priority =
            LAST_LOW_PRIORITY_PACKET_BEFORE_THE_OTHERS.saturating_add(1)..LOW_PRIORITY_PACKETS;
        let wire_order = first_low_priority
            .map(low_priority_payload)
            .chain((0..PACKETS).map(payload))
            .chain(rest_low_priority.map(low_priority_payload));
        let mut buf = [0u8; 2048];
        let mismatch = wire_order.enumerate().find_map(|(position, expected)| {
            capture
                .recv_matching_udp(
                    &mut buf,
                    &tx_loop_frame(links, expected.as_ref()),
                    Duration::from_secs(3),
                )
                .err()
                .map(|err| format!("frame {position} on the wire is not {expected:?}: {err}"))
        });
        // Ends the loop.
        drop(loop_sender.lock().unwrap().take());
        drop(sender);
        mismatch
    });
    assert_eq!(mismatch, None);
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn tx_loop_holds_low_priority_packets_while_frames_wait_on_the_device() {
    // In copy mode the kernel takes at most 32 frames from the TX ring per kick, so most of these
    // are still in the ring when the loop has written the last one.
    const PACKETS: usize = 200;
    // The loop takes a low priority packet only while fewer frames than this are written and not
    // yet completed.
    const MAX_WAITING: usize = 64;

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);
    let capture = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    capture
        .set_receive_buffer(8 << 20)
        .expect("grow the receive buffer of the raw packet receiver");
    let payload = Bytes::from_static(b"agave-xdp-tx-loop-waiting");
    let low_priority_payload = Bytes::from_static(b"agave-xdp-tx-loop-low-priority");

    let (sender, receiver) = tx_loop::channel(PACKETS, 1);
    for _ in 0..PACKETS {
        sender
            .try_send(tx_loop_packet(&links, links.right_ip, payload.clone()))
            .expect("queue packet");
    }
    sender
        .try_send_low_priority(tx_loop_packet(
            &links,
            links.right_ip,
            low_priority_payload.clone(),
        ))
        .expect("queue low priority packet");

    // How many of the packets were on the wire when the loop was done with the low priority
    // packet.
    let (sent_sender, sent_receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let (links, capture, payload) = (&links, &capture, &payload);
        scope.spawn(move || {
            run_tx_loop(links, receiver, |item| {
                if item.payload() == &low_priority_payload {
                    let sent = capture.count_received_udp(&tx_loop_frame(links, payload));
                    sent_sender.send(sent).expect("report the packets sent");
                }
            });
        });
        let sent = sent_receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("the loop takes the low priority packet");
        // Ends the loop.
        drop(sender);
        assert!(
            PACKETS.saturating_sub(sent) < MAX_WAITING,
            "only {sent} of {PACKETS} packets were on the wire when the loop was done with the \
             low priority packet"
        );
    });
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn tx_loop_sends_packets_after_one_low_priority_packet_it_cannot_send() {
    // Fewer than the loop commits at once, so they leave only when the loop commits them, and
    // no more than the kernel sends per kick in copy mode, so one commit sends them all.
    const PACKETS: usize = 8;
    const LOW_PRIORITY_PACKETS: usize = 4;

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);
    let capture = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let payload = Bytes::from_static(b"agave-xdp-tx-loop-not-held-back");
    // No route leads there, so the loop drops these packets without writing a frame.
    let unroutable = Ipv4Addr::new(203, 0, 113, 1);
    let low_priority_payload = |i: usize| Bytes::from(format!("low-priority-{i:03}"));

    let (sender, receiver) = tx_loop::channel(PACKETS, LOW_PRIORITY_PACKETS);
    for _ in 0..PACKETS {
        sender
            .try_send(tx_loop_packet(&links, links.right_ip, payload.clone()))
            .expect("queue packet");
    }
    for i in 0..LOW_PRIORITY_PACKETS {
        sender
            .try_send_low_priority(tx_loop_packet(&links, unroutable, low_priority_payload(i)))
            .expect("queue low priority packet");
    }

    // How many of the packets were on the wire when the loop was done with its second low
    // priority packet.
    let (sent_sender, sent_receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let (links, capture, payload) = (&links, &capture, &payload);
        let second = low_priority_payload(1);
        scope.spawn(move || {
            run_tx_loop(links, receiver, |item| {
                if item.payload() == &second {
                    // The loop commits nothing while it is in this callback, so waiting here
                    // only gives the kernel time to deliver what was committed before.
                    let start = Instant::now();
                    let mut sent = 0usize;
                    while sent < PACKETS && start.elapsed() < Duration::from_secs(1) {
                        let frame = tx_loop_frame(links, payload);
                        sent = sent.saturating_add(capture.count_received_udp(&frame));
                    }
                    sent_sender.send(sent).expect("report the packets sent");
                }
            });
        });
        let sent_before_second_low_priority_packet = sent_receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("the loop takes the low priority packets");
        // Ends the loop.
        drop(sender);
        assert_eq!(sent_before_second_low_priority_packet, PACKETS);
    });
}
