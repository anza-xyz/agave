//! The `recvmmsg` module provides recvmmsg() API implementation

pub use solana_perf::packet::PACKETS_PER_BATCH;
#[cfg(target_os = "linux")]
use {
    crate::msghdr::create_msghdr,
    itertools::izip,
    libc::{iovec, mmsghdr, sockaddr_storage, socklen_t, AF_INET, AF_INET6, MSG_WAITFORONE},
    std::{
        mem::{self, MaybeUninit},
        net::{SocketAddr, SocketAddrV4, SocketAddrV6},
        os::unix::io::AsRawFd,
    },
};
use {
    crate::packet::{Meta, RecvBuffer},
    solana_perf::packet::{BytesPacket, BytesPacketBatch},
    std::{io, net::UdpSocket},
};

#[cfg(not(target_os = "linux"))]
pub fn recv_mmsg(
    socket: &UdpSocket,
    packet_batch: &mut BytesPacketBatch,
    buffer: &mut RecvBuffer,
    is_staked_service: bool,
) -> io::Result</*num packets:*/ usize> {
    use solana_perf::packet::PACKET_DATA_SIZE;

    let count = packet_batch.capacity() - packet_batch.len();
    debug_assert!(
        buffer.capacity_packets() >= count,
        "expected `buffer` packet capacity ({}) to be greater or equal to
the `count` of packets we intend to receive ({})",
        buffer.capacity_packets(),
        count
    );

    let mut i = 0;
    for chunk_buffer in buffer.spare_capacity_chunks_mut().take(count) {
        // There is no `UdpSocket::recv*` method that works with `&mut [MaybeUninit<u8>]`.
        // They only accept initialized slices `&mut [u8]`.
        // Given that we never use non-Linux systems on production, so we don't
        // care about performance on them, let's just initailize the chunks of
        // `buffer` with zeros.
        for b in chunk_buffer.iter_mut() {
            b.write(0);
        }
        // SAFETY: All elements were initialized above.
        let chunk_buffer = unsafe { mem::transmute::<_, &mut [u8]>(chunk_buffer) };
        match socket.recv_from(chunk_buffer) {
            Err(_) if i > 0 => {
                break;
            }
            Err(e) => {
                return Err(e);
            }
            Ok((nrecv, from)) => {
                let mut meta = Meta::default();
                meta.size = nrecv;
                meta.set_socket_addr(&from);
                meta.set_from_staked_node(is_staked_service);
                if i == 0 {
                    socket.set_nonblocking(true)?;
                }
                // SAFETY: We are sure that the `nrecv` bytes of the buffer are
                // initialized.
                let chunk = unsafe { buffer.split_to_packet_bytes(nrecv) };
                packet_batch.push(BytesPacket::new(chunk, meta));
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

/** Receive multiple messages from `sock` into buffer provided in `packets`.
This is a wrapper around recvmmsg(7) call.

The buffer provided in packets should have all `meta()` fields cleared before calling
this function


 This function is *supposed to* timeout in 1 second and *may* block forever
 due to a bug in the linux kernel.
 You may want to call `sock.set_read_timeout(Some(Duration::from_secs(1)));` or similar
 prior to calling this function if you require this to actually time out after 1 second.
*/
#[cfg(target_os = "linux")]
pub fn recv_mmsg(
    sock: &UdpSocket,
    packet_batch: &mut BytesPacketBatch,
    buffer: &mut RecvBuffer,
    is_staked_service: bool,
) -> io::Result</*num packets:*/ usize> {
    // Should never hit this, but bail if the packet batch has no capacity.
    if packet_batch.capacity() == 0 {
        return Ok(0);
    }
    let count = packet_batch.capacity() - packet_batch.len();
    debug_assert!(
        buffer.capacity_packets() >= count,
        "expected `buffer` packet capacity ({}) to be greater or equal to
the `count` of packets we intend to receive ({})",
        buffer.capacity_packets(),
        count
    );
    const SOCKADDR_STORAGE_SIZE: socklen_t = mem::size_of::<sockaddr_storage>() as socklen_t;

    let mut iovs = [MaybeUninit::uninit(); PACKETS_PER_BATCH];
    let mut addrs = [MaybeUninit::zeroed(); PACKETS_PER_BATCH];
    let mut hdrs = [MaybeUninit::uninit(); PACKETS_PER_BATCH];

    let sock_fd = sock.as_raw_fd();

    for (buffer, hdr, iov, addr) in izip!(
        buffer.spare_capacity_chunks_mut(),
        &mut hdrs,
        &mut iovs,
        &mut addrs
    )
    .take(count)
    {
        iov.write(iovec {
            iov_base: buffer.as_mut_ptr() as *mut libc::c_void,
            iov_len: buffer.len(),
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
    for (addr, hdr) in izip!(addrs, hdrs).take(nrecv) {
        // SAFETY: We initialized `count` elements of `hdrs` above. `count` is
        // passed to recvmmsg() as the limit of messages that can be read. So,
        // `nrevc <= count` which means we initialized this `hdr` and
        // recvmmsg() will have updated it appropriately
        let hdr_ref = unsafe { hdr.assume_init_ref() };
        // SAFETY: Similar to above, we initialized this `addr` and recvmmsg()
        // will have populated it
        let addr_ref = unsafe { addr.assume_init_ref() };
        let size = hdr_ref.msg_len as usize;
        // SAFETY: We are sure that the `msg_len` bytes of the buffer are
        // initialized.
        let chunk = unsafe { buffer.split_to_packet_bytes(size) };
        let mut meta = Meta::default();
        meta.size = size;
        if let Some(addr) = cast_socket_addr(addr_ref, hdr_ref) {
            meta.set_socket_addr(&addr);
        }
        meta.set_from_staked_node(is_staked_service);
        packet_batch.push(BytesPacket::new(chunk, meta));
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
        crate::{packet::PACKET_DATA_SIZE, recvmmsg::*},
        solana_net_utils::sockets::{
            bind_in_range_with_config, localhost_port_range_for_tests, unique_port_range_for_tests,
            SocketConfiguration as SocketConfig,
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
            let mut buffer = RecvBuffer::new(TEST_NUM_MSGS);
            let recv = recv_mmsg(&reader, &mut packets, &mut buffer, false).unwrap();
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
            let sent = TEST_NUM_MSGS + 10;
            for _ in 0..sent {
                let data = [0; PACKET_DATA_SIZE];
                sender.send_to(&data[..], addr).unwrap();
            }

            let mut packets = BytesPacketBatch::with_capacity(TEST_NUM_MSGS);
            let mut buffer = RecvBuffer::new(TEST_NUM_MSGS);
            let recv = recv_mmsg(&reader, &mut packets, &mut buffer, false).unwrap();
            assert_eq!(TEST_NUM_MSGS, recv);
            for packet in packets.iter().take(recv) {
                assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
                assert_eq!(packet.meta().socket_addr(), saddr);
            }

            packets.clear();
            buffer.reserve_packets(TEST_NUM_MSGS);
            let recv = recv_mmsg(&reader, &mut packets, &mut buffer, false).unwrap();
            assert_eq!(sent - TEST_NUM_MSGS, recv);
            for packet in packets.iter().take(recv) {
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
        let mut packets = BytesPacketBatch::with_capacity(TEST_NUM_MSGS);
        let mut buffer = RecvBuffer::new(TEST_NUM_MSGS);
        let recv = recv_mmsg(&reader, &mut packets, &mut buffer, false).unwrap();
        assert_eq!(TEST_NUM_MSGS, recv);
        for packet in packets.iter().take(recv) {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender_addr);
        }
        reader.set_nonblocking(true).unwrap();

        packets.clear();
        buffer.reserve_packets(TEST_NUM_MSGS);
        let _recv = recv_mmsg(&reader, &mut packets, &mut buffer, false);
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
        let sent1 = TEST_NUM_MSGS - 1;

        let sender2 = bind_in_range_with_config(ip, port_range, SocketConfig::default())
            .unwrap()
            .1;
        let sender_addr = sender2.local_addr().unwrap();
        let sent2 = TEST_NUM_MSGS + 1;

        for _ in 0..sent1 {
            let data = [0; PACKET_DATA_SIZE];
            sender1.send_to(&data[..], reader_addr).unwrap();
        }

        for _ in 0..sent2 {
            let data = [0; PACKET_DATA_SIZE];
            sender2.send_to(&data[..], reader_addr).unwrap();
        }

        let mut packets = BytesPacketBatch::with_capacity(TEST_NUM_MSGS);
        let mut buffer = RecvBuffer::new(TEST_NUM_MSGS);

        let recv = recv_mmsg(&reader, &mut packets, &mut buffer, false).unwrap();
        assert_eq!(TEST_NUM_MSGS, recv);
        for packet in packets.iter().take(sent1) {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender1_addr);
        }
        for packet in packets.iter().skip(sent1).take(recv - sent1) {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender_addr);
        }

        packets.clear();
        buffer.reserve_packets(TEST_NUM_MSGS);
        let recv = recv_mmsg(&reader, &mut packets, &mut buffer, false).unwrap();
        assert_eq!(sent1 + sent2 - TEST_NUM_MSGS, recv);
        for packet in packets.iter().take(recv) {
            assert_eq!(packet.meta().size, PACKET_DATA_SIZE);
            assert_eq!(packet.meta().socket_addr(), sender_addr);
        }
    }
}
