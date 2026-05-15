#![cfg(target_os = "linux")]

mod common;

use {
    agave_xdp::{
        device::{NetworkDevice, QueueId},
        netlink::MacAddress,
        socket::Socket,
        transmitter::{BytesTxPacket, TransmitterBuilder, XdpConfig},
        umem::{OwnedUmem, PageAlignedMemory},
    },
    bytes::Bytes,
    std::{
        io, mem,
        net::{Ipv4Addr, SocketAddr, SocketAddrV4},
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

fn transmitter_cpu() -> Option<usize> {
    let cores = core_affinity::get_core_ids()?;
    if cores.len() < 2 {
        eprintln!("skipping transmitter smoke test: at least two CPU cores are required");
        return None;
    }
    cores.first().map(|core| core.id)
}

struct PacketSocket {
    fd: OwnedFd,
}

impl PacketSocket {
    fn bind(if_index: u32) -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                (libc::ETH_P_ALL as u16).to_be() as i32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let addr = libc::sockaddr_ll {
            sll_family: libc::AF_PACKET as u16,
            sll_protocol: (libc::ETH_P_ALL as u16).to_be(),
            sll_ifindex: if_index as i32,
            sll_hatype: 0,
            sll_pkttype: 0,
            sll_halen: 0,
            sll_addr: [0; 8],
        };
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    fn recv_matching_udp(
        &self,
        expected: &ExpectedUdpPacket<'_>,
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        self.recv_matching_payload("matching UDP frame", timeout, |frame| {
            matching_udp_payload(frame, expected)
        })
    }

    fn recv_matching_gre_udp(
        &self,
        expected: &ExpectedGreUdpPacket<'_>,
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        self.recv_matching_payload("matching GRE UDP frame", timeout, |frame| {
            matching_gre_udp_payload(frame, expected)
        })
    }

    fn recv_matching_payload<F>(
        &self,
        description: &str,
        timeout: Duration,
        mut matcher: F,
    ) -> io::Result<Vec<u8>>
    where
        F: for<'a> FnMut(&'a [u8]) -> Option<&'a [u8]>,
    {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "timeout overflows instant")
        })?;
        let mut frame = [0u8; 2048];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for {description}"),
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let mut pfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe {
                libc::poll(
                    &mut pfd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if rc == 0 {
                continue;
            }

            let len = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    frame.as_mut_ptr() as *mut libc::c_void,
                    frame.len(),
                    0,
                )
            };
            if len < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            let frame = &frame[..len as usize];
            if let Some(payload) = matcher(frame) {
                return Ok(payload.to_vec());
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

fn matching_udp_payload<'a>(frame: &'a [u8], expected: &ExpectedUdpPacket<'_>) -> Option<&'a [u8]> {
    const ETH_HEADER_SIZE: usize = 14;

    if frame.len() < ETH_HEADER_SIZE {
        return None;
    }
    if frame[0..6] != expected.dst_mac.0 || frame[6..12] != expected.src_mac.0 {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != libc::ETH_P_IP as u16 {
        return None;
    }

    matching_ipv4_udp_payload(
        &frame[ETH_HEADER_SIZE..],
        &ExpectedUdpDatagram {
            src_ip: expected.src_ip,
            dst_ip: expected.dst_ip,
            src_port: expected.src_port,
            dst_port: expected.dst_port,
            payload: expected.payload,
        },
    )
}

fn matching_gre_udp_payload<'a>(
    frame: &'a [u8],
    expected: &ExpectedGreUdpPacket<'_>,
) -> Option<&'a [u8]> {
    const ETH_HEADER_SIZE: usize = 14;
    const IPV4_MIN_HEADER_SIZE: usize = 20;
    const GRE_HEADER_SIZE: usize = 4;
    const GRE_FLAGS_VERSION_BASIC: u16 = 0;

    if frame.len() < ETH_HEADER_SIZE.checked_add(IPV4_MIN_HEADER_SIZE)? {
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
        .checked_add(GRE_HEADER_SIZE)?
        .checked_add(IPV4_MIN_HEADER_SIZE)?;
    if outer_ihl < IPV4_MIN_HEADER_SIZE || frame.len() < min_frame_len {
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

    let inner_offset = gre_offset.checked_add(GRE_HEADER_SIZE)?;
    matching_ipv4_udp_payload(
        frame.get(inner_offset..)?,
        &ExpectedUdpDatagram {
            src_ip: expected.inner_src_ip,
            dst_ip: expected.inner_dst_ip,
            src_port: expected.src_port,
            dst_port: expected.dst_port,
            payload: expected.payload,
        },
    )
}

fn matching_ipv4_udp_payload<'a>(
    ip: &'a [u8],
    expected: &ExpectedUdpDatagram<'_>,
) -> Option<&'a [u8]> {
    const IPV4_MIN_HEADER_SIZE: usize = 20;
    const UDP_HEADER_SIZE: usize = 8;

    let min_udp_len = IPV4_MIN_HEADER_SIZE.checked_add(UDP_HEADER_SIZE)?;
    if ip.len() < min_udp_len {
        return None;
    }

    let ihl = usize::from(ip[0] & 0x0f).checked_mul(4)?;
    let min_packet_len = ihl.checked_add(UDP_HEADER_SIZE)?;
    if ihl < IPV4_MIN_HEADER_SIZE || ip.len() < min_packet_len {
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
    let payload = &udp[UDP_HEADER_SIZE..udp_len];
    (payload == expected.payload).then_some(payload)
}

#[test]
fn socket_tx_binds_copy_mode_to_veth_queue() {
    let _netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();

    let dev = NetworkDevice::new(&links.left_name).expect("open veth device");
    let queue = dev.open_queue(QueueId(0)).expect("open device queue");
    let frame_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let memory = PageAlignedMemory::alloc(frame_size, 128).expect("allocate xdp umem");
    let umem = OwnedUmem::new(memory, frame_size as u32).expect("create xdp umem");

    let (socket, tx) =
        Socket::tx(queue, umem, false, 64, 64).expect("bind copy-mode AF_XDP TX socket");
    assert_eq!(socket.queue().if_index(), links.left_if_index);
    assert_eq!(socket.queue().id().0, 0);

    let ring = tx.ring.expect("TX-only socket must expose a TX ring");
    assert_eq!(ring.capacity(), 64);
    assert_eq!(ring.available(), 64);
}

#[test]
fn transmitter_sends_udp_payload_over_veth_in_copy_mode() {
    let Some(cpu_id) = transmitter_cpu() else {
        return;
    };

    let _netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();
    common::replace_neighbor(links.right_ip, links.right_mac, &links.left_name);

    let receiver = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let dst_port = 45_678;
    let src_port = 12_345;
    let destination = SocketAddr::V4(SocketAddrV4::new(links.right_ip, dst_port));
    let payload = Bytes::from_static(b"agave-xdp-transmitter-smoke");

    let exit = Arc::new(AtomicBool::new(false));
    let mut config = XdpConfig::new(Some(links.left_name.clone()), vec![cpu_id], false);
    config.tx_channel_cap = 16;

    let (transmitter, sender) = TransmitterBuilder::new(config, Arc::clone(&exit))
        .expect("build copy-mode transmitter")
        .build();

    let packet = BytesTxPacket::new(
        SocketAddrV4::new(links.left_ip, src_port),
        destination,
        payload.clone(),
    );
    sender
        .try_send(0, packet)
        .expect("queue packet through XdpSender::try_send");

    let received = receiver
        .recv_matching_udp(
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

    exit.store(true, Ordering::Relaxed);
    drop(sender);
    transmitter.join().expect("join transmitter threads");
}

#[test]
fn transmitter_sends_udp_payload_over_gre_tunnel_in_copy_mode() {
    let Some(cpu_id) = transmitter_cpu() else {
        return;
    };

    let _netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();
    common::replace_neighbor(links.right_ip, links.right_mac, &links.left_name);
    common::add_route_to_dev(&format!("{}/32", links.right_ip), &links.left_name);
    let gre = common::setup_gre_tunnel(&links);
    common::add_route_to_dev_with_src("192.0.2.0/24", &gre.name, gre.overlay_ip);

    let receiver = PacketSocket::bind(links.right_if_index).expect("bind raw packet receiver");
    let dst_port = 45_679;
    let src_port = 12_346;
    let overlay_destination = Ipv4Addr::new(192, 0, 2, 99);
    let destination = SocketAddr::V4(SocketAddrV4::new(overlay_destination, dst_port));
    let payload = Bytes::from_static(b"agave-xdp-transmitter-gre-smoke");

    let exit = Arc::new(AtomicBool::new(false));
    let mut config = XdpConfig::new(Some(links.left_name.clone()), vec![cpu_id], false);
    config.tx_channel_cap = 16;

    let (transmitter, sender) = TransmitterBuilder::new(config, Arc::clone(&exit))
        .expect("build copy-mode transmitter")
        .build();

    let packet = BytesTxPacket::new(
        SocketAddrV4::new(links.left_ip, src_port),
        destination,
        payload.clone(),
    );
    sender
        .try_send(0, packet)
        .expect("queue packet through XdpSender::try_send");

    let received = receiver
        .recv_matching_gre_udp(
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

    exit.store(true, Ordering::Relaxed);
    drop(sender);
    transmitter.join().expect("join transmitter threads");
}
