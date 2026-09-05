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
    /// Fatal IO error during send, the socket can not be used any more.
    #[error("fatal IO error, the send path is broken")]
    IoError(io::Error),
}

impl From<SendPktsError> for TransportError {
    fn from(err: SendPktsError) -> Self {
        Self::Custom(format!("{err:?}"))
    }
}

/// Decide whether a failed send should abort the whole batch.
///
/// Only errors that make the socket itself unusable are fatal. Everything else is a
/// problem with one destination or transient backpressure. Those are expected in
/// normal operation and only cost us this one packet. On non-unix platforms we can not
/// tell the two apart, so we swallow everything.
fn check_fatal(err: io::Error) -> Result<(), SendPktsError> {
    #[cfg(unix)]
    match err.raw_os_error() {
        Some(libc::EMSGSIZE) => {
            debug_assert!(
                false,
                "packet exceeds the maximum UDP datagram size: {err:?}"
            );
        }
        Some(
            libc::EBADF
            | libc::ENOTSOCK
            | libc::EFAULT
            | libc::EPIPE
            | libc::EOPNOTSUPP
            | libc::EDESTADDRREQ,
        ) => {
            return Err(SendPktsError::IoError(err));
        }
        _ => (),
    }
    #[cfg(not(unix))]
    let _ = err;
    Ok(())
}

/// See the linux implementation of [`batch_send`].
// The type and lifetime constraints are overspecified to match 'linux' code.
#[cfg(not(target_os = "linux"))]
pub fn batch_send<'a, S, T: 'a + ?Sized>(
    sock: &UdpSocket,
    packets: impl IntoIterator<Item = (&'a T, S), IntoIter: ExactSizeIterator>,
) -> Result</*num_sent:*/ usize, SendPktsError>
where
    S: Borrow<SocketAddr>,
    &'a T: AsRef<[u8]>,
{
    let mut num_sent = 0;
    for (p, a) in packets {
        match sock.send_to(p.as_ref(), a.borrow()) {
            Ok(_) => num_sent += 1,
            Err(err) => check_fatal(err)?,
        }
    }
    Ok(num_sent)
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
fn sendmmsg_retry(
    sock: &UdpSocket,
    hdrs: &mut [mmsghdr],
) -> Result</*num_sent:*/ usize, SendPktsError> {
    let sock_fd = sock.as_raw_fd();
    let mut num_sent = 0;

    let mut pkts = &mut *hdrs;
    while !pkts.is_empty() {
        let npkts = match unsafe { libc::sendmmsg(sock_fd, &mut pkts[0], pkts.len() as u32, 0) } {
            -1 => {
                check_fatal(io::Error::last_os_error())?;
                // skip over the failing packet
                1_usize
            }
            n => {
                // if we fail to send all packets we advance to the failing
                // packet and retry in order to capture the error code
                num_sent += n as usize;
                n as usize
            }
        };
        pkts = &mut pkts[npkts..];
    }

    Ok(num_sent)
}

#[cfg(target_os = "linux")]
const MAX_IOV: usize = libc::UIO_MAXIOV as usize;

#[cfg(target_os = "linux")]
fn batch_send_max_iov<'a, S, T: 'a + ?Sized>(
    sock: &UdpSocket,
    packets: impl IntoIterator<Item = (&'a T, S), IntoIter: ExactSizeIterator>,
) -> Result</*num_sent:*/ usize, SendPktsError>
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

/// Send every `(packet, destination)` pair over `sock`.
///
/// Returns the number of packets that were sent successfully. Failures for individual
/// destinations are expected and are not reported. An Err is returned only when the
/// send failed for a reason that makes the socket permanently unusable.
// Need &'a to ensure that raw packet pointers obtained in mmsghdr_for_packet
// stay valid.
#[cfg(target_os = "linux")]
pub fn batch_send<'a, S, T: 'a + ?Sized>(
    sock: &UdpSocket,
    packets: impl IntoIterator<Item = (&'a T, S), IntoIter: ExactSizeIterator>,
) -> Result</*num_sent:*/ usize, SendPktsError>
where
    S: Borrow<SocketAddr>,
    &'a T: AsRef<[u8]>,
{
    let mut packets = packets.into_iter();
    let mut num_sent = 0;
    loop {
        let chunk = packets.by_ref().take(MAX_IOV);
        if chunk.len() == 0 {
            break;
        }
        // On error the socket is dead, do not bother with the remaining chunks.
        num_sent += batch_send_max_iov(sock, chunk)?;
    }
    Ok(num_sent)
}

/// Send the same `packet` to every destination in `dests`.
///
/// Shares the semantics of [`batch_send`]: unreachable destinations only lower
/// the returned count, an error means the socket is permanently unusable.
pub fn multi_target_send<S, T>(
    sock: &UdpSocket,
    packet: T,
    dests: &[S],
) -> Result</*num_sent:*/ usize, SendPktsError>
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
            packet::{BytesPacketBatch, Packet},
            recvmmsg::recv_mmsg,
            sendmmsg::{SendPktsError, batch_send, multi_target_send},
        },
        assert_matches::assert_matches,
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_packet::PACKET_DATA_SIZE,
        std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    };

    #[test]
    pub fn test_send_mmsg_one_dest() {
        let reader = bind_to_localhost_unique().expect("should bind - reader");
        let addr = reader.local_addr().unwrap();
        let sender = bind_to_localhost_unique().expect("should bind - sender");

        let packets: Vec<_> = (0..32).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let packet_refs: Vec<_> = packets.iter().map(|p| (&p[..], &addr)).collect();

        let num_sent = batch_send(&sender, packet_refs).expect("socket should be usable");
        assert_eq!(num_sent, 32);

        let mut packets = BytesPacketBatch::with_capacity(32);
        let recv = recv_mmsg(&reader, &mut packets, &mut Vec::new()).unwrap();
        assert_eq!(32, recv);
    }

    #[test]
    pub fn test_send_mmsg_multi_dest() {
        let reader = bind_to_localhost_unique().expect("should bind - reader 1");
        let addr = reader.local_addr().unwrap();

        let reader2 = bind_to_localhost_unique().expect("should bind - reader 2");
        let addr2 = reader2.local_addr().unwrap();

        let sender = bind_to_localhost_unique().expect("should bind - sender");

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

        let num_sent = batch_send(&sender, packet_refs).expect("socket should be usable");
        assert_eq!(num_sent, 32);

        let mut packets = BytesPacketBatch::with_capacity(32);
        let recv = recv_mmsg(&reader, &mut packets, &mut Vec::new()).unwrap();
        assert_eq!(16, recv);

        let mut packets = BytesPacketBatch::with_capacity(32);
        let recv = recv_mmsg(&reader2, &mut packets, &mut Vec::new()).unwrap();
        assert_eq!(16, recv);
    }

    #[test]
    pub fn test_multicast_msg() {
        let reader = bind_to_localhost_unique().expect("should bind - reader 1");
        let addr = reader.local_addr().unwrap();

        let reader2 = bind_to_localhost_unique().expect("should bind - reader 2");
        let addr2 = reader2.local_addr().unwrap();

        let reader3 = bind_to_localhost_unique().expect("should bind - reader 3");
        let addr3 = reader3.local_addr().unwrap();

        let reader4 = bind_to_localhost_unique().expect("should bind - reader 4");
        let addr4 = reader4.local_addr().unwrap();

        let sender = bind_to_localhost_unique().expect("should bind - reader 5");

        let packet = Packet::default();

        let num_sent = multi_target_send(
            &sender,
            packet.data(..).unwrap(),
            &[&addr, &addr2, &addr3, &addr4],
        )
        .expect("socket should be usable");
        assert_eq!(num_sent, 4);

        let mut packets = BytesPacketBatch::with_capacity(32);
        let recv = recv_mmsg(&reader, &mut packets, &mut Vec::new()).unwrap();
        assert_eq!(1, recv);

        let mut packets = BytesPacketBatch::with_capacity(32);
        let recv = recv_mmsg(&reader2, &mut packets, &mut Vec::new()).unwrap();
        assert_eq!(1, recv);

        let mut packets = BytesPacketBatch::with_capacity(32);
        let recv = recv_mmsg(&reader3, &mut packets, &mut Vec::new()).unwrap();
        assert_eq!(1, recv);

        let mut packets = BytesPacketBatch::with_capacity(32);
        let recv = recv_mmsg(&reader4, &mut packets, &mut Vec::new()).unwrap();
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

        let sender = bind_to_localhost_unique().expect("should bind - sender");
        assert_matches!(
            batch_send(&sender, packet_refs),
            Ok(/*num_sent:*/ 2),
            "a destination with a mismatched IP version must be skipped, not fatal"
        );
        assert_matches!(
            multi_target_send(&sender, &packets[0], &dest_refs),
            Ok(/*num_sent:*/ 2),
            "a destination with a mismatched IP version must be skipped, not fatal"
        );
    }

    #[test]
    fn test_intermediate_failures_unreachable_address() {
        let packets: Vec<_> = (0..5).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let ipv4local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let ipv4broadcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 8080);
        let sender = bind_to_localhost_unique().expect("should bind - sender");

        // test intermediate failures for batch_send
        let packet_refs: Vec<_> = vec![
            (&packets[0][..], &ipv4local),
            (&packets[1][..], &ipv4broadcast),
            (&packets[2][..], &ipv4local),
            (&packets[3][..], &ipv4broadcast),
            (&packets[4][..], &ipv4local),
        ];
        assert_matches!(
            batch_send(&sender, packet_refs),
            Ok(/*num_sent:*/ 3),
            "unreachable destinations in the middle of a batch must be skipped, not fatal"
        );

        // test leading and trailing failures for batch_send
        let packet_refs: Vec<_> = vec![
            (&packets[0][..], &ipv4broadcast),
            (&packets[1][..], &ipv4local),
            (&packets[2][..], &ipv4broadcast),
            (&packets[3][..], &ipv4local),
            (&packets[4][..], &ipv4broadcast),
        ];
        assert_matches!(
            batch_send(&sender, packet_refs),
            Ok(/*num_sent:*/ 2),
            "unreachable destinations at the edges of a batch must be skipped, not fatal"
        );

        // test consecutive intermediate failures for batch_send
        let packet_refs: Vec<_> = vec![
            (&packets[0][..], &ipv4local),
            (&packets[1][..], &ipv4local),
            (&packets[2][..], &ipv4broadcast),
            (&packets[3][..], &ipv4broadcast),
            (&packets[4][..], &ipv4local),
        ];
        assert_matches!(
            batch_send(&sender, packet_refs),
            Ok(/*num_sent:*/ 3),
            "consecutive unreachable destinations must be skipped, not fatal"
        );

        // test intermediate failures for multi_target_send
        let dest_refs: Vec<_> = vec![
            &ipv4local,
            &ipv4broadcast,
            &ipv4local,
            &ipv4broadcast,
            &ipv4local,
        ];
        assert_matches!(
            multi_target_send(&sender, &packets[0], &dest_refs),
            Ok(/*num_sent:*/ 3),
            "unreachable destinations in the middle of a batch must be skipped, not fatal"
        );

        // test leading and trailing failures for multi_target_send
        let dest_refs: Vec<_> = vec![
            &ipv4broadcast,
            &ipv4local,
            &ipv4broadcast,
            &ipv4local,
            &ipv4broadcast,
        ];
        assert_matches!(
            multi_target_send(&sender, &packets[0], &dest_refs),
            Ok(/*num_sent:*/ 2),
            "unreachable destinations at the edges of a batch must be skipped, not fatal"
        );
    }

    #[test]
    fn test_all_destinations_unreachable() {
        let packets: Vec<_> = (0..3).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let ipv4broadcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 8080);
        let sender = bind_to_localhost_unique().expect("should bind - sender");

        let packet_refs: Vec<_> = packets.iter().map(|p| (&p[..], &ipv4broadcast)).collect();
        assert_matches!(
            batch_send(&sender, packet_refs),
            Ok(/*num_sent:*/ 0),
            "a usable socket must not report an error even if no destination is reachable"
        );

        let dest_refs: Vec<_> = vec![&ipv4broadcast, &ipv4broadcast, &ipv4broadcast];
        assert_matches!(
            multi_target_send(&sender, &packets[0], &dest_refs),
            Ok(/*num_sent:*/ 0),
            "a usable socket must not report an error even if no destination is reachable"
        );
    }

    /// A socket that is not actually a socket makes every send fail with
    /// `ENOTSOCK`, which is permanent and must surface as an error.
    #[test]
    #[cfg(unix)]
    fn test_fatal_error_not_a_socket() {
        use std::{
            fs::File,
            os::fd::{FromRawFd, IntoRawFd},
        };

        let devnull = File::open("/dev/null").expect("should open /dev/null");
        // SAFETY: `into_raw_fd` transfers ownership of a valid, open fd, so the
        // resulting `UdpSocket` is the sole owner and closes it exactly once.
        let not_a_socket = unsafe { UdpSocket::from_raw_fd(devnull.into_raw_fd()) };

        let packets: Vec<_> = (0..3).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let packet_refs: Vec<_> = packets.iter().map(|p| (&p[..], &addr)).collect();

        match batch_send(&not_a_socket, packet_refs) {
            Ok(num_sent) => panic!("a send on a non-socket fd must fail, sent {num_sent}"),
            Err(SendPktsError::IoError(ioerror)) => {
                assert_eq!(ioerror.raw_os_error(), Some(libc::ENOTSOCK));
            }
        }
    }
}
