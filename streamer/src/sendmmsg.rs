//! The `sendmmsg` module provides sendmmsg() API implementation

#[cfg(target_os = "linux")]
use {
    crate::msghdr::create_msghdr,
    itertools::izip,
    libc::{iovec, mmsghdr, sockaddr_in, sockaddr_in6, sockaddr_storage, socklen_t},
    std::{
        mem::{self, MaybeUninit},
        os::unix::io::AsRawFd,
        ptr,
    },
};
use {
    solana_transaction_error::TransportError,
    std::{
        borrow::Borrow,
        io,
        net::{SocketAddr, UdpSocket},
    },
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum SendPktsError {
    /// IO Error during send: first error, num failed packets
    #[error("IO Error, some packets could not be sent")]
    IoError(io::Error, usize),
}

impl From<SendPktsError> for TransportError {
    fn from(err: SendPktsError) -> Self {
        Self::Custom(format!("{err:?}"))
    }
}

// The type and lifetime constraints are overspecified to match 'linux' code.
#[cfg(not(target_os = "linux"))]
pub fn batch_send<'a, S, T: 'a + ?Sized>(
    sock: &UdpSocket,
    packets: impl IntoIterator<Item = (&'a T, S), IntoIter: ExactSizeIterator>,
) -> Result<(), SendPktsError>
where
    S: Borrow<SocketAddr>,
    &'a T: AsRef<[u8]>,
{
    let mut num_failed = 0;
    let mut erropt = None;
    for (p, a) in packets {
        if let Err(e) = sock.send_to(p.as_ref(), a.borrow()) {
            num_failed += 1;
            if erropt.is_none() {
                erropt = Some(e);
            }
        }
    }

    if let Some(err) = erropt {
        Err(SendPktsError::IoError(err, num_failed))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn mmsghdr_for_packet(
    packet: &[u8],
    dest: &SocketAddr,
    iov: &mut MaybeUninit<iovec>,
    addr: &mut MaybeUninit<sockaddr_storage>,
    hdr: &mut MaybeUninit<mmsghdr>,
) {
    const SIZE_OF_SOCKADDR_IN: usize = mem::size_of::<sockaddr_in>();
    const SIZE_OF_SOCKADDR_IN6: usize = mem::size_of::<sockaddr_in6>();
    const SIZE_OF_SOCKADDR_STORAGE: usize = mem::size_of::<sockaddr_storage>();
    const SOCKADDR_IN_PADDING: usize = SIZE_OF_SOCKADDR_STORAGE - SIZE_OF_SOCKADDR_IN;
    const SOCKADDR_IN6_PADDING: usize = SIZE_OF_SOCKADDR_STORAGE - SIZE_OF_SOCKADDR_IN6;

    iov.write(iovec {
        iov_base: packet.as_ptr() as *mut libc::c_void,
        iov_len: packet.len(),
    });

    let msg_namelen = match dest {
        SocketAddr::V4(socket_addr_v4) => {
            let ptr: *mut sockaddr_in = addr.as_mut_ptr() as *mut _;
            unsafe {
                ptr::write(
                    ptr,
                    *nix::sys::socket::SockaddrIn::from(*socket_addr_v4).as_ref(),
                );
                // Zero the remaining bytes after sockaddr_in
                ptr::write_bytes(
                    (ptr as *mut u8).add(SIZE_OF_SOCKADDR_IN),
                    0,
                    SOCKADDR_IN_PADDING,
                );
            }
            SIZE_OF_SOCKADDR_IN as socklen_t
        }
        SocketAddr::V6(socket_addr_v6) => {
            let ptr: *mut sockaddr_in6 = addr.as_mut_ptr() as *mut _;
            unsafe {
                ptr::write(
                    ptr,
                    *nix::sys::socket::SockaddrIn6::from(*socket_addr_v6).as_ref(),
                );
                // Zero the remaining bytes after sockaddr_in6
                ptr::write_bytes(
                    (ptr as *mut u8).add(SIZE_OF_SOCKADDR_IN6),
                    0,
                    SOCKADDR_IN6_PADDING,
                );
            }
            SIZE_OF_SOCKADDR_IN6 as socklen_t
        }
    };

    let msg_hdr = create_msghdr(addr, msg_namelen, iov);

    hdr.write(mmsghdr {
        msg_len: 0,
        msg_hdr,
    });
}

#[cfg(target_os = "linux")]
fn sendmmsg_retry(sock: &UdpSocket, hdrs: &mut [mmsghdr]) -> Result<(), SendPktsError> {
    let sock_fd = sock.as_raw_fd();
    let mut total_sent = 0;
    let mut erropt = None;

    let mut pkts = &mut *hdrs;
    while !pkts.is_empty() {
        let npkts = match unsafe { libc::sendmmsg(sock_fd, &mut pkts[0], pkts.len() as u32, 0) } {
            -1 => {
                if erropt.is_none() {
                    erropt = Some(io::Error::last_os_error());
                }
                // skip over the failing packet
                1_usize
            }
            n => {
                // if we fail to send all packets we advance to the failing
                // packet and retry in order to capture the error code
                total_sent += n as usize;
                n as usize
            }
        };
        pkts = &mut pkts[npkts..];
    }

    if let Some(err) = erropt {
        Err(SendPktsError::IoError(err, hdrs.len() - total_sent))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
const MAX_IOV: usize = libc::UIO_MAXIOV as usize;

#[cfg(target_os = "linux")]
fn batch_send_max_iov<'a, S, T: 'a + ?Sized>(
    sock: &UdpSocket,
    packets: impl IntoIterator<Item = (&'a T, S), IntoIter: ExactSizeIterator>,
) -> Result<(), SendPktsError>
where
    S: Borrow<SocketAddr>,
    &'a T: AsRef<[u8]>,
{
    let packets = packets.into_iter();
    let num_packets = packets.len();
    debug_assert!(num_packets <= MAX_IOV);

    let mut iovs = [MaybeUninit::uninit(); MAX_IOV];
    let mut addrs = [MaybeUninit::uninit(); MAX_IOV];
    let mut hdrs = [MaybeUninit::uninit(); MAX_IOV];

    // izip! will iterate packets.len() times, leaving hdrs, iovs, and addrs initialized only up to packets.len()
    for ((pkt, dest), hdr, iov, addr) in izip!(packets, &mut hdrs, &mut iovs, &mut addrs) {
        mmsghdr_for_packet(pkt.as_ref(), dest.borrow(), iov, addr, hdr);
    }

    // SAFETY: The first `packets.len()` elements of `hdrs`, `iovs`, and `addrs` are
    // guaranteed to be initialized by `mmsghdr_for_packet` before this loop.
    let hdrs_slice =
        unsafe { std::slice::from_raw_parts_mut(hdrs.as_mut_ptr() as *mut mmsghdr, num_packets) };

    let result = sendmmsg_retry(sock, hdrs_slice);

    // SAFETY: The first `packets.len()` elements of `hdrs`, `iovs`, and `addrs` are
    // guaranteed to be initialized by `mmsghdr_for_packet` before this loop.
    for (hdr, iov, addr) in izip!(&mut hdrs, &mut iovs, &mut addrs).take(num_packets) {
        unsafe {
            hdr.assume_init_drop();
            iov.assume_init_drop();
            addr.assume_init_drop();
        }
    }

    result
}

// Need &'a to ensure that raw packet pointers obtained in mmsghdr_for_packet
// stay valid.
#[cfg(target_os = "linux")]
pub fn batch_send<'a, S, T: 'a + ?Sized>(
    sock: &UdpSocket,
    packets: impl IntoIterator<Item = (&'a T, S), IntoIter: ExactSizeIterator>,
) -> Result<(), SendPktsError>
where
    S: Borrow<SocketAddr>,
    &'a T: AsRef<[u8]>,
{
    let mut packets = packets.into_iter();
    loop {
        let chunk = packets.by_ref().take(MAX_IOV);
        if chunk.len() == 0 {
            break;
        }
        batch_send_max_iov(sock, chunk)?;
    }
    Ok(())
}

pub fn multi_target_send<S, T>(
    sock: &UdpSocket,
    packet: T,
    dests: &[S],
) -> Result<(), SendPktsError>
where
    S: Borrow<SocketAddr>,
    T: AsRef<[u8]>,
{
    let dests = dests.iter().map(Borrow::borrow);
    let pkts = dests.map(|addr| (&packet, addr));
    batch_send(sock, pkts)
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            packet::Packet,
            recvmmsg::recv_mmsg,
            sendmmsg::{batch_send, multi_target_send, SendPktsError},
        },
        assert_matches::assert_matches,
        solana_net_utils::{bind_to_localhost, bind_to_unspecified},
        solana_packet::PACKET_DATA_SIZE,
        std::{
            io::ErrorKind,
            net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        },
    };

    #[test]
    pub fn test_send_mmsg_one_dest() {
        let reader = bind_to_localhost().expect("bind");
        let addr = reader.local_addr().unwrap();
        let sender = bind_to_localhost().expect("bind");

        let packets: Vec<_> = (0..32).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let packet_refs: Vec<_> = packets.iter().map(|p| (&p[..], &addr)).collect();

        let sent = batch_send(&sender, packet_refs).ok();
        assert_eq!(sent, Some(()));

        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(&reader, &mut packets[..]).unwrap();
        assert_eq!(32, recv);
    }

    #[test]
    pub fn test_send_mmsg_multi_dest() {
        let reader = bind_to_localhost().expect("bind");
        let addr = reader.local_addr().unwrap();

        let reader2 = bind_to_localhost().expect("bind");
        let addr2 = reader2.local_addr().unwrap();

        let sender = bind_to_localhost().expect("bind");

        let packets: Vec<_> = (0..32).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let packet_refs: Vec<_> = packets
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if i < 16 {
                    (&p[..], &addr)
                } else {
                    (&p[..], &addr2)
                }
            })
            .collect();

        let sent = batch_send(&sender, packet_refs).ok();
        assert_eq!(sent, Some(()));

        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(&reader, &mut packets[..]).unwrap();
        assert_eq!(16, recv);

        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(&reader2, &mut packets[..]).unwrap();
        assert_eq!(16, recv);
    }

    #[test]
    pub fn test_multicast_msg() {
        let reader = bind_to_localhost().expect("bind");
        let addr = reader.local_addr().unwrap();

        let reader2 = bind_to_localhost().expect("bind");
        let addr2 = reader2.local_addr().unwrap();

        let reader3 = bind_to_localhost().expect("bind");
        let addr3 = reader3.local_addr().unwrap();

        let reader4 = bind_to_localhost().expect("bind");
        let addr4 = reader4.local_addr().unwrap();

        let sender = bind_to_localhost().expect("bind");

        let packet = Packet::default();

        let sent = multi_target_send(
            &sender,
            packet.data(..).unwrap(),
            &[&addr, &addr2, &addr3, &addr4],
        )
        .ok();
        assert_eq!(sent, Some(()));

        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(&reader, &mut packets[..]).unwrap();
        assert_eq!(1, recv);

        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(&reader2, &mut packets[..]).unwrap();
        assert_eq!(1, recv);

        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(&reader3, &mut packets[..]).unwrap();
        assert_eq!(1, recv);

        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(&reader4, &mut packets[..]).unwrap();
        assert_eq!(1, recv);
    }

    #[test]
    fn test_intermediate_failures_mismatched_bind() {
        let packets: Vec<_> = (0..3).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let ip4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let ip6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080);
        let packet_refs: Vec<_> = vec![
            (&packets[0][..], &ip4),
            (&packets[1][..], &ip6),
            (&packets[2][..], &ip4),
        ];
        let dest_refs: Vec<_> = vec![&ip4, &ip6, &ip4];

        let sender = bind_to_unspecified().expect("bind");
        let res = batch_send(&sender, packet_refs);
        assert_matches!(res, Err(SendPktsError::IoError(_, /*num_failed*/ 1)));
        let res = multi_target_send(&sender, &packets[0], &dest_refs);
        assert_matches!(res, Err(SendPktsError::IoError(_, /*num_failed*/ 1)));
    }

    #[test]
    fn test_intermediate_failures_unreachable_address() {
        let packets: Vec<_> = (0..5).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let ipv4local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let ipv4broadcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 8080);
        let sender = bind_to_unspecified().expect("bind");

        // test intermediate failures for batch_send
        let packet_refs: Vec<_> = vec![
            (&packets[0][..], &ipv4local),
            (&packets[1][..], &ipv4broadcast),
            (&packets[2][..], &ipv4local),
            (&packets[3][..], &ipv4broadcast),
            (&packets[4][..], &ipv4local),
        ];
        match batch_send(&sender, packet_refs) {
            Ok(()) => panic!(),
            Err(SendPktsError::IoError(ioerror, num_failed)) => {
                assert_matches!(ioerror.kind(), ErrorKind::PermissionDenied);
                assert_eq!(num_failed, 2);
            }
        }

        // test leading and trailing failures for batch_send
        let packet_refs: Vec<_> = vec![
            (&packets[0][..], &ipv4broadcast),
            (&packets[1][..], &ipv4local),
            (&packets[2][..], &ipv4broadcast),
            (&packets[3][..], &ipv4local),
            (&packets[4][..], &ipv4broadcast),
        ];
        match batch_send(&sender, packet_refs) {
            Ok(()) => panic!(),
            Err(SendPktsError::IoError(ioerror, num_failed)) => {
                assert_matches!(ioerror.kind(), ErrorKind::PermissionDenied);
                assert_eq!(num_failed, 3);
            }
        }

        // test consecutive intermediate failures for batch_send
        let packet_refs: Vec<_> = vec![
            (&packets[0][..], &ipv4local),
            (&packets[1][..], &ipv4local),
            (&packets[2][..], &ipv4broadcast),
            (&packets[3][..], &ipv4broadcast),
            (&packets[4][..], &ipv4local),
        ];
        match batch_send(&sender, packet_refs) {
            Ok(()) => panic!(),
            Err(SendPktsError::IoError(ioerror, num_failed)) => {
                assert_matches!(ioerror.kind(), ErrorKind::PermissionDenied);
                assert_eq!(num_failed, 2);
            }
        }

        // test intermediate failures for multi_target_send
        let dest_refs: Vec<_> = vec![
            &ipv4local,
            &ipv4broadcast,
            &ipv4local,
            &ipv4broadcast,
            &ipv4local,
        ];
        match multi_target_send(&sender, &packets[0], &dest_refs) {
            Ok(()) => panic!(),
            Err(SendPktsError::IoError(ioerror, num_failed)) => {
                assert_matches!(ioerror.kind(), ErrorKind::PermissionDenied);
                assert_eq!(num_failed, 2);
            }
        }

        // test leading and trailing failures for multi_target_send
        let dest_refs: Vec<_> = vec![
            &ipv4broadcast,
            &ipv4local,
            &ipv4broadcast,
            &ipv4local,
            &ipv4broadcast,
        ];
        match multi_target_send(&sender, &packets[0], &dest_refs) {
            Ok(()) => panic!(),
            Err(SendPktsError::IoError(ioerror, num_failed)) => {
                assert_matches!(ioerror.kind(), ErrorKind::PermissionDenied);
                assert_eq!(num_failed, 3);
            }
        }
    }
}
