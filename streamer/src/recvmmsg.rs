//! The `recvmmsg` module provides recvmmsg() API implementation

#[cfg(target_os = "linux")]
use {
    crate::msghdr::create_msghdr,
    itertools::izip,
    libc::{AF_INET, AF_INET6, MSG_WAITFORONE, iovec, mmsghdr, sockaddr_storage, socklen_t},
    std::{
        mem::{self, MaybeUninit},
        net::{SocketAddr, SocketAddrV4, SocketAddrV6},
        os::unix::io::AsRawFd,
    },
};
use {
    crate::packet::{BytesPacketBatch, Meta, PACKET_DATA_SIZE},
    bytes::BytesMut,
    solana_perf::packet::{BytesPacket, PACKETS_PER_BATCH},
    std::{io, net::UdpSocket},
};

/// Pool of receive buffers for [`recv_mmsg`].
///
/// `recvmmsg(2)` has to be handed a buffer for every packet it is allowed to return, but it
/// may return fewer if socket buffer is drained. The buffers it did not fill are kept here
/// instead of being dropped.
///
/// Buffers that got used for a packet leave the pool, so it must be replenished periodically.
pub(crate) struct PacketBufferPool(Vec<BytesMut>);

impl PacketBufferPool {
    /// Allocates the buffers for one full [`recv_mmsg`] call.
    pub(crate) fn new() -> Self {
        let mut pool = Self(Vec::with_capacity(PACKETS_PER_BATCH));
        pool.replenish();
        pool
    }

    /// Replaces the buffers consumed by the packets received since the last call. The call
    /// will allocate only as many buffers as were consumed since last call, so it is idempotent.
    fn replenish(&mut self) {
        // we never need to worry about pool being > PACKETS_PER_BATCH since this is
        // the only place where we add elements to it.
        self.0.resize_with(PACKETS_PER_BATCH, || {
            BytesMut::with_capacity(PACKET_DATA_SIZE)
        });
    }
}

/// Fallback for platforms without `recvmmsg(2)`, see the linux implementation for the
/// contract. One `recvfrom(2)` per packet, so this one only ever takes a buffer out of
/// `pool` when it has a packet to put in it.
#[cfg(not(target_os = "linux"))]
pub(crate) fn recv_mmsg(
    socket: &UdpSocket,
    packets: &mut BytesPacketBatch,
    pool: &mut PacketBufferPool,
) -> io::Result</*num packets:*/ usize> {
    pool.replenish();
    let mut i = 0;
    let count = PACKETS_PER_BATCH.saturating_sub(packets.len());
    packets.reserve(count);
    for _ in 0..count {
        // Receive into a buffer that is still owned by the pool and only take it out once it
        // holds a packet, so that no error path can forget to put it back.
        let buffer = pool
            .0
            .last_mut()
            .expect("count is bounded by the pool size");
        // Within the capacity every pooled buffer is built with, so this does not allocate.
        buffer.resize(PACKET_DATA_SIZE, 0);
        match socket.recv_from(buffer) {
            Err(_) if i > 0 => break,
            Err(e) => return Err(e),
            Ok((nrecv, from)) => {
                if i == 0 {
                    socket.set_nonblocking(true)?;
                }
                let mut buffer = pool.0.pop().expect("the receive buffer is still pooled");
                buffer.truncate(nrecv);
                let mut meta = Meta::default();
                meta.size = nrecv;
                meta.set_socket_addr(&from);
                packets.push(BytesPacket::new(buffer.freeze(), meta));
            }
        }
        i += 1;
    }
    Ok(i)
}

#[cfg(target_os = "linux")]
fn cast_socket_addr(addr: &sockaddr_storage, hdr: &mmsghdr) -> Option<SocketAddr> {
    use libc::{sa_family_t, sockaddr_in, sockaddr_in6};
    const SOCKADDR_IN_SIZE: usize = std::mem::size_of::<sockaddr_in>();
    const SOCKADDR_IN6_SIZE: usize = std::mem::size_of::<sockaddr_in6>();
    if addr.ss_family == AF_INET as sa_family_t
        && hdr.msg_hdr.msg_namelen == SOCKADDR_IN_SIZE as socklen_t
    {
        // ref: https://github.com/rust-lang/socket2/blob/65085d9dff270e588c0fbdd7217ec0b392b05ef2/src/sockaddr.rs#L167-L172
        let addr = unsafe { &*(addr as *const _ as *const sockaddr_in) };
        return Some(SocketAddr::V4(SocketAddrV4::new(
            std::net::Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
            u16::from_be(addr.sin_port),
        )));
    }
    if addr.ss_family == AF_INET6 as sa_family_t
        && hdr.msg_hdr.msg_namelen == SOCKADDR_IN6_SIZE as socklen_t
    {
        // ref: https://github.com/rust-lang/socket2/blob/65085d9dff270e588c0fbdd7217ec0b392b05ef2/src/sockaddr.rs#L174-L189
        let addr = unsafe { &*(addr as *const _ as *const sockaddr_in6) };
        return Some(SocketAddr::V6(SocketAddrV6::new(
            std::net::Ipv6Addr::from(addr.sin6_addr.s6_addr),
            u16::from_be(addr.sin6_port),
            addr.sin6_flowinfo,
            addr.sin6_scope_id,
        )));
    }
    error!(
        "recvmmsg unexpected ss_family:{} msg_namelen:{}",
        addr.ss_family, hdr.msg_hdr.msg_namelen
    );
    None
}

/** Receive multiple messages from `sock`, appending them to `packets`.
This is a wrapper around recvmmsg(7) call.

Packets are appended until `packets` holds `PACKETS_PER_BATCH` packets. The batch is
never cleared and is grown as needed, so the caller can fill up partial batches.
Returns the number of packets appended.

Receive buffers are taken from `pool` if available there, pool is replenished
automatically as buffers are consumed.

 This function is *supposed to* timeout in 1 second and *may* block forever
 due to a bug in the linux kernel.
 You may want to call `sock.set_read_timeout(Some(Duration::from_secs(1)));` or similar
 prior to calling this function if you require this to actually time out after 1 second.
*/
#[cfg(target_os = "linux")]
pub(crate) fn recv_mmsg(
    sock: &UdpSocket,
    packets: &mut BytesPacketBatch,
    pool: &mut PacketBufferPool,
) -> io::Result</*num packets:*/ usize> {
    pool.replenish();
    let count = PACKETS_PER_BATCH.saturating_sub(packets.len());
    // Should never hit this, but bail if the batch handed to us is already full
    if count == 0 {
        return Ok(0);
    }
    packets.reserve(count);
    const SOCKADDR_STORAGE_SIZE: socklen_t = mem::size_of::<sockaddr_storage>() as socklen_t;

    let mut iovs = [MaybeUninit::uninit(); PACKETS_PER_BATCH];
    let mut addrs = [MaybeUninit::zeroed(); PACKETS_PER_BATCH];
    let mut hdrs = [MaybeUninit::uninit(); PACKETS_PER_BATCH];

    let sock_fd = sock.as_raw_fd();

    for (hdr, iov, addr, buffer) in
        izip!(&mut hdrs, &mut iovs, &mut addrs, pool.0.iter_mut()).take(count)
    {
        iov.write(iovec {
            iov_base: buffer.as_mut_ptr() as *mut libc::c_void,
            iov_len: PACKET_DATA_SIZE,
        });

        let msg_hdr = create_msghdr(addr, SOCKADDR_STORAGE_SIZE, iov);

        hdr.write(mmsghdr {
            msg_len: 0,
            msg_hdr,
        });
    }

    let mut ts = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    // TODO: remove .try_into().unwrap() once rust libc fixes recvmmsg types for musl
    #[allow(clippy::useless_conversion)]
    let nrecv = unsafe {
        libc::recvmmsg(
            sock_fd,
            hdrs[0].assume_init_mut(),
            count as u32,
            MSG_WAITFORONE.try_into().unwrap(),
            &mut ts,
        )
    };
    let nrecv = if nrecv < 0 {
        return Err(io::Error::last_os_error());
    } else {
        usize::try_from(nrecv).unwrap()
    };
    // Consume the buffers from the pool matching number of received packets.
    for (addr, hdr, mut buffer) in izip!(addrs, hdrs, pool.0.drain(..nrecv)) {
        // SAFETY: We initialized `count` elements of `hdrs` above. `count` is
        // passed to recvmmsg() as the limit of messages that can be read. So,
        // `nrevc <= count` which means we initialized this `hdr` and
        // recvmmsg() will have updated it appropriately
        let hdr_ref = unsafe { hdr.assume_init_ref() };
        // SAFETY: Similar to above, we initialized this `addr` and recvmmsg()
        // will have populated it
        let addr_ref = unsafe { addr.assume_init_ref() };
        let msg_len = hdr_ref.msg_len as usize;
        // SAFETY: `recvmmsg` wrote `msg_len` initialized bytes into the buffer.
        unsafe { buffer.set_len(msg_len) };
        let mut meta = Meta::default();
        meta.size = msg_len;
        if let Some(addr) = cast_socket_addr(addr_ref, hdr_ref) {
            meta.set_socket_addr(&addr);
        }
        packets.push(BytesPacket::new(buffer.freeze(), meta));
    }

    for (iov, addr, hdr) in izip!(&mut iovs, &mut addrs, &mut hdrs).take(count) {
        // SAFETY: We initialized `count` elements of each array above
        //
        // It may be that `packets.len() != PACKETS_PER_BATCH`; thus, some elements
        // in `iovs` / `addrs` / `hdrs` may not get initialized. So, we must
        // manually drop `count` elements from each array instead of being able
        // to convert [MaybeUninit<T>] to [T] and letting `Drop` do the work
        // for us when these items go out of scope at the end of the function
        unsafe {
            iov.assume_init_drop();
            addr.assume_init_drop();
            hdr.assume_init_drop();
        }
    }

    Ok(nrecv)
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            packet::{BytesPacketBatch, PACKET_DATA_SIZE},
            recvmmsg::*,
        },
        solana_net_utils::sockets::{
            SocketConfiguration as SocketConfig, bind_in_range_with_config,
            localhost_port_range_for_tests, unique_port_range_for_tests,
        },
        std::{
            net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
            time::{Duration, Instant},
        },
    };

    type TestConfig = (UdpSocket, SocketAddr, UdpSocket, SocketAddr);

    fn test_setup_reader_sender(ip: IpAddr) -> io::Result<TestConfig> {
        let port_range = unique_port_range_for_tests(2);
        let reader = bind_in_range_with_config(
            ip,
            (port_range.start, port_range.end),
            SocketConfig::default(),
        )?
        .1;
        let reader_addr = reader.local_addr()?;
        let sender = bind_in_range_with_config(
            ip,
            (port_range.start, port_range.end),
            SocketConfig::default(),
        )?
        .1;
        let sender_addr = sender.local_addr()?;
        Ok((reader, reader_addr, sender, sender_addr))
    }

    const TEST_NUM_MSGS: usize = 32;
    #[test]
    pub fn test_recv_mmsg_one_iter() {
        let test_one_iter = |(reader, addr, sender, saddr): TestConfig| {
            let sent = TEST_NUM_MSGS - 1;
            for _ in 0..sent {
                let data = [0; PACKET_DATA_SIZE];
                sender.send_to(&data[..], addr).unwrap();
            }

            let mut packets = BytesPacketBatch::with_capacity(TEST_NUM_MSGS);
            let recv = recv_mmsg(&reader, &mut packets, &mut PacketBufferPool::new()).unwrap();
            assert_eq!(sent, recv);
            for packet in packets.iter() {
                assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
                assert_eq!(packet.meta().socket_addr(), saddr);
            }
        };

        test_one_iter(test_setup_reader_sender(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap());

        match test_setup_reader_sender(IpAddr::V6(Ipv6Addr::LOCALHOST)) {
            Ok(config) => test_one_iter(config),
            Err(e) => warn!("Failed to configure IPv6: {e:?}"),
        }
    }

    #[test]
    pub fn test_recv_mmsg_multi_iter() {
        let test_multi_iter = |(reader, addr, sender, saddr): TestConfig| {
            // Send more than a single call can return, so that the leftovers stay
            // queued for the second call.
            let sent = PACKETS_PER_BATCH + 10;
            for _ in 0..sent {
                let data = [0; PACKET_DATA_SIZE];
                sender.send_to(&data[..], addr).unwrap();
            }

            let mut pool = PacketBufferPool::new();
            let mut packets = BytesPacketBatch::with_capacity(PACKETS_PER_BATCH);
            let recv = recv_mmsg(&reader, &mut packets, &mut pool).unwrap();
            assert_eq!(PACKETS_PER_BATCH, recv);
            for packet in packets.iter() {
                assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
                assert_eq!(packet.meta().socket_addr(), saddr);
            }

            packets.clear();
            let recv = recv_mmsg(&reader, &mut packets, &mut pool).unwrap();
            assert_eq!(sent - PACKETS_PER_BATCH, recv);
            for packet in packets.iter() {
                assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
                assert_eq!(packet.meta().socket_addr(), saddr);
            }
        };

        test_multi_iter(test_setup_reader_sender(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap());

        match test_setup_reader_sender(IpAddr::V6(Ipv6Addr::LOCALHOST)) {
            Ok(config) => test_multi_iter(config),
            Err(e) => warn!("Failed to configure IPv6: {e:?}"),
        }
    }

    #[test]
    pub fn test_recv_mmsg_multi_iter_timeout() {
        let (reader, reader_addr, sender, sender_addr) =
            test_setup_reader_sender(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        reader.set_read_timeout(Some(Duration::new(5, 0))).unwrap();
        reader.set_nonblocking(false).unwrap();
        let sent = TEST_NUM_MSGS;
        for _ in 0..sent {
            let data = [0; PACKET_DATA_SIZE];
            sender.send_to(&data[..], reader_addr).unwrap();
        }

        let start = Instant::now();
        let mut pool = PacketBufferPool::new();
        let mut packets = BytesPacketBatch::with_capacity(TEST_NUM_MSGS);
        let recv = recv_mmsg(&reader, &mut packets, &mut pool).unwrap();
        assert_eq!(TEST_NUM_MSGS, recv);
        for packet in packets.iter() {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender_addr);
        }
        reader.set_nonblocking(true).unwrap();

        packets.clear();
        let _recv = recv_mmsg(&reader, &mut packets, &mut pool);
        assert!(start.elapsed().as_secs() < 5);
    }

    #[test]
    pub fn test_recv_mmsg_multi_addrs() {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let reader = bind_in_range_with_config(ip, port_range, SocketConfig::default())
            .unwrap()
            .1;
        let reader_addr = reader.local_addr().unwrap();
        let sender1 = bind_in_range_with_config(ip, port_range, SocketConfig::default())
            .unwrap()
            .1;
        let sender1_addr = sender1.local_addr().unwrap();
        let sent1 = PACKETS_PER_BATCH - 1;

        let sender2 = bind_in_range_with_config(ip, port_range, SocketConfig::default())
            .unwrap()
            .1;
        let sender_addr = sender2.local_addr().unwrap();
        // sent1 + sent2 is one over PACKETS_PER_BATCH, so one packet of the second
        // sender's traffic is left queued for the second call.
        let sent2 = 2;

        for _ in 0..sent1 {
            let data = [0; PACKET_DATA_SIZE];
            sender1.send_to(&data[..], reader_addr).unwrap();
        }

        for _ in 0..sent2 {
            let data = [0; PACKET_DATA_SIZE];
            sender2.send_to(&data[..], reader_addr).unwrap();
        }
        let mut pool = PacketBufferPool::new();
        let mut packets = BytesPacketBatch::with_capacity(PACKETS_PER_BATCH);

        let recv = recv_mmsg(&reader, &mut packets, &mut pool).unwrap();
        assert_eq!(PACKETS_PER_BATCH, recv);
        for packet in packets.iter().take(sent1) {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender1_addr);
        }
        for packet in packets.iter().skip(sent1) {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender_addr);
        }

        packets.clear();
        let recv = recv_mmsg(&reader, &mut packets, &mut pool).unwrap();
        assert_eq!(sent1 + sent2 - PACKETS_PER_BATCH, recv);
        for packet in packets.iter() {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender_addr);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    pub fn test_recv_mmsg_reuses_pool_buffers() {
        let (reader, reader_addr, sender, _sender_addr) =
            test_setup_reader_sender(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let sent = 2;
        for _ in 0..sent {
            let data = [0; PACKET_DATA_SIZE];
            sender.send_to(&data[..], reader_addr).unwrap();
        }

        let mut pool = PacketBufferPool::new();
        let mut packets = BytesPacketBatch::with_capacity(PACKETS_PER_BATCH);
        let recv = recv_mmsg(&reader, &mut packets, &mut pool).unwrap();
        assert_eq!(sent, recv);
        assert_eq!(
            pool.0.len(),
            PACKETS_PER_BATCH - sent,
            "buffers that did not receive a packet must be left in the pool"
        );

        // A second call tops the pool back up by exactly the buffers the first one consumed.
        for _ in 0..sent {
            let data = [0; PACKET_DATA_SIZE];
            sender.send_to(&data[..], reader_addr).unwrap();
        }
        let recv = recv_mmsg(&reader, &mut packets, &mut pool).unwrap();
        assert_eq!(sent, recv);
        assert_eq!(
            pool.0.len(),
            PACKETS_PER_BATCH - sent,
            "the pool must not grow past the buffers needed for one call"
        );
    }
}
