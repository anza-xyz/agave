//! The `packet` module defines data structures and methods to pull data from the network.
use {
    crate::{recvmmsg::recv_mmsg, socket::SocketAddrSpace},
    std::{
        io::Result,
        net::UdpSocket,
        time::{Duration, Instant},
    },
};
pub use {
    solana_packet::{Meta, Packet, PacketMut, PACKET_DATA_SIZE},
    solana_perf::packet::{
        to_packet_batches, BufMut, PacketBatchRecycler, PacketMutBatch, PacketRead, NUM_PACKETS,
        PACKETS_PER_BATCH,
    },
};

pub fn recv_from(
    batch: &mut Vec<PacketMut>,
    socket: &UdpSocket,
    max_wait: Duration,
) -> Result<usize> {
    let mut i = 0;
    //DOCUMENTED SIDE-EFFECT
    //Performance out of the IO without poll
    //  * block on the socket until it's readable
    //  * set the socket to non blocking
    //  * read until it fails
    //  * set it back to blocking before returning
    socket.set_nonblocking(false)?;
    trace!("receiving on {}", socket.local_addr().unwrap());
    let start = Instant::now();
    loop {
        match recv_mmsg(socket, &mut batch[i..]) {
            Err(_) if i > 0 => {
                if start.elapsed() > max_wait {
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
                if start.elapsed() > max_wait || i >= PACKETS_PER_BATCH {
                    break;
                }
            }
        }
    }
    batch.truncate(i);
    Ok(i)
}

pub fn send_to(
    batch: &[Packet],
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
        bytes::BufMut,
        solana_net_utils::bind_to_localhost,
        std::io::{self, Write},
    };

    #[test]
    pub fn packet_send_recv() {
        solana_logger::setup();
        let recv_socket = bind_to_localhost().expect("bind");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = bind_to_localhost().expect("bind");
        let saddr = send_socket.local_addr().unwrap();

        let packet_batch_size = 10;
        let mut mut_batch = vec![PacketMut::default(); packet_batch_size];

        for m in mut_batch.iter_mut() {
            m.put_slice(b"ibrl");
            m.meta_mut().set_socket_addr(&addr);
        }

        let mut batch = Vec::with_capacity(packet_batch_size);
        for packet in mut_batch {
            batch.push(packet.freeze());
        }

        send_to(&batch, &send_socket, &SocketAddrSpace::Unspecified).unwrap();

        let mut batch = PacketMutBatch::with_len(packet_batch_size);
        let recvd = recv_from(
            &mut batch,
            &recv_socket,
            Duration::from_millis(1), // max_wait
        )
        .unwrap();
        assert_eq!(recvd, batch.len());

        for m in batch.iter() {
            assert_eq!(m.data(..).unwrap(), b"ibrl");
            assert_eq!(m.len(), 4);
            assert_eq!(m.meta().socket_addr(), saddr);
        }
    }

    #[test]
    pub fn debug_trait() {
        write!(io::sink(), "{:?}", Packet::default()).unwrap();
        write!(io::sink(), "{:?}", Vec::<Packet>::default()).unwrap();
    }

    #[test]
    fn test_packet_resize() {
        solana_logger::setup();
        let recv_socket = bind_to_localhost().expect("bind");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = bind_to_localhost().expect("bind");
        let mut batch = PacketMutBatch::default();

        // Should only get PACKETS_PER_BATCH packets per iteration even
        // if a lot more were sent, and regardless of packet size
        for _ in 0..2 * PACKETS_PER_BATCH {
            let batch_size = 1;
            let mut batch = Vec::with_capacity(batch_size);
            batch.resize(batch_size, Packet::default());
            for p in batch.iter_mut() {
                p.meta_mut().set_socket_addr(&addr);
            }
            send_to(&batch, &send_socket, &SocketAddrSpace::Unspecified).unwrap();
        }
        let recvd = recv_from(
            &mut batch,
            &recv_socket,
            Duration::from_millis(100), // max_wait
        )
        .unwrap();
        // Check we only got PACKETS_PER_BATCH packets
        assert_eq!(recvd, PACKETS_PER_BATCH);
        assert_eq!(batch.capacity(), PACKETS_PER_BATCH);
    }
}
