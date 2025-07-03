//! The `packet` module defines data structures and methods to pull data from the network.
use {
    crate::{
        recvmmsg::{recv_mmsg, RecvBuffer, RecvMetas},
        socket::SocketAddrSpace,
    },
    solana_perf::packet::{BytesPacket, BytesPacketBatch},
    std::{
        io::{ErrorKind, Result},
        net::UdpSocket,
        time::{Duration, Instant},
    },
};
pub use {
    solana_packet::{Meta, Packet, PACKET_DATA_SIZE},
    solana_perf::packet::{
        PacketBatch, PacketBatchRecycler, PacketRef, PacketRefMut, PinnedPacketBatch, NUM_PACKETS,
        PACKETS_PER_BATCH,
    },
};

/** Receive multiple messages from `sock` into buffer provided in `batch`.
This is a wrapper around recvmmsg(7) call.

 This function is *supposed to* timeout in 1 second and *may* block forever
 due to a bug in the linux kernel.
 You may want to call `sock.set_read_timeout(Some(Duration::from_secs(1)));` or similar
 prior to calling this function if you require this to actually time out after 1 second.
*/
pub(crate) fn recv_from(
    socket: &UdpSocket,
    buffer: &mut RecvBuffer,
    metas: &mut RecvMetas,
    // If max_wait is None, reads from the socket until either:
    //   * 64 packets are read (NUM_RCVMMSGS == PACKETS_PER_BATCH == 64), or
    //   * There are no more data available to read from the socket.
    max_wait: Option<Duration>,
    is_staked_service: bool,
) -> Result<BytesPacketBatch> {
    let mut i = 0;
    let mut buffers = buffer.chunk_bufs();
    //DOCUMENTED SIDE-EFFECT
    //Performance out of the IO without poll
    //  * block on the socket until it's readable
    //  * set the socket to non blocking
    //  * read until it fails
    //  * set it back to blocking before returning
    socket.set_nonblocking(false)?;
    trace!("receiving on {}", socket.local_addr().unwrap());
    let should_wait = max_wait.is_some();
    let start = should_wait.then(Instant::now);
    loop {
        match recv_mmsg(socket, &mut buffers[i..], metas) {
            Err(err) if i > 0 => {
                if !should_wait && err.kind() == ErrorKind::WouldBlock {
                    break;
                }
            }
            Err(e) => {
                trace!("recv_from err {:?}", e);
                return Err(e);
            }
            Ok(npkts) => {
                if i == 0 {
                    socket.set_nonblocking(true)?;
                }
                trace!("got {} packets", npkts);
                i += npkts;
                // Try to batch into big enough buffers
                // will cause less re-shuffling later on.
                if i >= PACKETS_PER_BATCH {
                    break;
                }
            }
        }
        if start.as_ref().map(Instant::elapsed) > max_wait {
            break;
        }
    }

    drop(buffers);
    let packet_batch: BytesPacketBatch = metas
        .iter()
        .map(|recv_meta| {
            // Split off a chunk with the full packet capacity.
            let mut chunk = buffer.split_to(PACKET_DATA_SIZE).freeze();
            // Truncate it to the length of the received message.
            chunk.truncate(recv_meta.size());

            let mut meta = Meta {
                size: chunk.len(),
                ..Default::default()
            };
            if let Some(socket_addr) = recv_meta.socket_addr() {
                meta.set_socket_addr(&socket_addr);
            }
            meta.set_from_staked_node(is_staked_service);

            BytesPacket::new(chunk, meta)
        })
        .collect();

    Ok(packet_batch)
}

pub fn send_to(
    batch: &PacketBatch,
    socket: &UdpSocket,
    socket_addr_space: &SocketAddrSpace,
) -> Result<()> {
    for p in batch.iter() {
        let addr = p.meta().socket_addr();
        if socket_addr_space.check(&addr) {
            if let Some(data) = p.data(..) {
                socket.send_to(data, addr)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        bytes::Bytes,
        solana_net_utils::bind_to_localhost,
        solana_perf::packet::{BytesPacket, BytesPacketBatch},
        std::{
            io::{self, Write},
            net::SocketAddr,
        },
    };

    #[test]
    fn test_packets_set_addr() {
        // test that the address is actually being updated
        let send_addr: SocketAddr = "127.0.0.1:123".parse().unwrap();
        let packets = vec![Packet::default()];
        let mut packet_batch = PinnedPacketBatch::new(packets);
        packet_batch.set_addr(&send_addr);
        assert_eq!(packet_batch[0].meta().socket_addr(), send_addr);
    }

    #[test]
    pub fn packet_send_recv() {
        solana_logger::setup();
        let recv_socket = bind_to_localhost().expect("bind");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = bind_to_localhost().expect("bind");
        let saddr = send_socket.local_addr().unwrap();

        const PACKET_BATCH_SIZE: usize = 10;
        let buf = Bytes::from(vec![0; PACKET_DATA_SIZE]);
        let mut meta = Meta::default();
        meta.set_socket_addr(&addr);
        let batch = vec![BytesPacket::new(buf, meta); PACKET_BATCH_SIZE];
        let batch = PacketBatch::from(BytesPacketBatch::from(batch));

        send_to(&batch, &send_socket, &SocketAddrSpace::Unspecified).unwrap();

        let mut buffer = RecvBuffer::new();
        let mut metas = RecvMetas::new();
        let recvd = recv_from(
            &recv_socket,
            &mut buffer,
            &mut metas,
            Some(Duration::from_millis(1)), // max_wait
            true,
        )
        .unwrap();
        assert_eq!(recvd.len(), batch.len());

        for m in recvd.iter() {
            assert_eq!(m.data(..).unwrap().len(), PACKET_DATA_SIZE);
            assert_eq!(m.meta().size, PACKET_DATA_SIZE);
            assert_eq!(m.meta().socket_addr(), saddr);
        }
    }

    #[test]
    pub fn debug_trait() {
        write!(io::sink(), "{:?}", Packet::default()).unwrap();
        write!(io::sink(), "{:?}", PinnedPacketBatch::default()).unwrap();
    }

    #[test]
    fn test_packet_partial_eq() {
        let mut p1 = Packet::default();
        let mut p2 = Packet::default();

        p1.meta_mut().size = 1;
        p1.buffer_mut()[0] = 0;

        p2.meta_mut().size = 1;
        p2.buffer_mut()[0] = 0;

        assert!(p1 == p2);

        p2.buffer_mut()[0] = 4;
        assert!(p1 != p2);
    }

    #[test]
    fn test_packet_resize() {
        solana_logger::setup();
        let recv_socket = bind_to_localhost().expect("bind");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = bind_to_localhost().expect("bind");

        // Should only get PACKETS_PER_BATCH packets per iteration even
        // if a lot more were sent, and regardless of packet size
        for _ in 0..2 * PACKETS_PER_BATCH {
            let buf = Bytes::from(vec![0; 1]);
            let mut meta = Meta::default();
            meta.set_socket_addr(&addr);
            let batch = vec![BytesPacket::new(buf, meta); PACKETS_PER_BATCH];
            let batch = PacketBatch::from(BytesPacketBatch::from(batch));
            send_to(&batch, &send_socket, &SocketAddrSpace::Unspecified).unwrap();
        }
        let mut buffer = RecvBuffer::new();
        let mut metas = RecvMetas::new();
        let recvd = recv_from(
            &recv_socket,
            &mut buffer,
            &mut metas,
            Some(Duration::from_millis(100)), // max_wait
            true,
        )
        .unwrap();
        // Check we only got PACKETS_PER_BATCH packets
        assert_eq!(recvd.len(), PACKETS_PER_BATCH);
    }
}
