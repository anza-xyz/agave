#![cfg(target_os = "linux")]

mod common;

use {
    agave_cpu_utils::cpu_affinity,
    agave_xdp::{
        gre::packet::GRE_HEADER_BASE_SIZE,
        netlink::MacAddress,
        packet::{ETH_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE},
        transmitter::{
            BytesTxPacket, QueueCpuBinding, Transmitter, TransmitterBuilder, XdpConfig, XdpSender,
        },
    },
    bytes::Bytes,
    nix::{
        errno::Errno,
        poll::{PollFd, PollFlags, PollTimeout, poll},
        sys::socket::{
            AddressFamily, MsgFlags, SockFlag, SockProtocol, SockType, SockaddrLike,
            SockaddrStorage, bind, recv, socket,
        },
    },
    std::{
        io, mem,
        net::{Ipv4Addr, SocketAddr, SocketAddrV4},
        ops::Range,
        os::fd::{AsFd, AsRawFd, OwnedFd},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

fn transmitter_cpu() -> usize {
    let cores = cpu_affinity(None).expect("linux provides affine cores");
    assert!(
        cores.len() >= 2,
        "transmitter smoke test requires at least 2 affine CPU cores, found {}",
        cores.len(),
    );
    **cores.first().expect("at least two affine cores")
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

#[test]
#[ignore = "requires root and network namespace privileges"]
fn transmitter_sends_udp_payload_over_veth_in_copy_mode() {
    let cpu_id = transmitter_cpu();

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair();
    common::replace_neighbor(links.right_ip, links.right_mac, common::LEFT_IFACE);

    let receiver = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let dst_port = 45_678;
    let src_port = 12_345;
    let destination = SocketAddr::V4(SocketAddrV4::new(links.right_ip, dst_port));
    let payload = Bytes::from_static(b"agave-xdp-transmitter-smoke");

    let exit = Arc::new(AtomicBool::new(false));
    let mut config = XdpConfig::new(
        Some(common::LEFT_IFACE.to_string()),
        vec![QueueCpuBinding {
            queue: 0,
            cpu: cpu_id,
        }],
        false,
    );
    config.tx_channel_cap = 16;

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
fn transmitter_sends_udp_payload_over_gre_tunnel_in_copy_mode() {
    let cpu_id = transmitter_cpu();

    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair();
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
    let mut config = XdpConfig::new(
        Some(common::LEFT_IFACE.to_string()),
        vec![QueueCpuBinding {
            queue: 0,
            cpu: cpu_id,
        }],
        false,
    );
    config.tx_channel_cap = 16;

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
