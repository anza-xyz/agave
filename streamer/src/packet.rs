//! The `packet` module defines data structures and methods to pull data from the network.
#[cfg(unix)]
use nix::poll::{poll, PollFd, PollTimeout};
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
))]
use nix::{poll::ppoll, sys::time::TimeSpec};
use {
    crate::recvmmsg::recv_mmsg,
    solana_net_utils::SocketAddrSpace,
    std::{
        io::{ErrorKind, Result},
        net::UdpSocket,
        time::{Duration, Instant},
    },
};
pub use {
    solana_packet::{Meta, Packet, PACKET_DATA_SIZE},
    solana_perf::packet::{
        PacketBatch, PacketBatchRecycler, PacketRef, PacketRefMut, RecycledPacketBatch,
        NUM_PACKETS, PACKETS_PER_BATCH,
    },
};

/** Receive multiple messages from `sock` into buffer provided in `batch`.
This is a wrapper around recvmmsg(7) call.

 This function is *supposed to* timeout in 1 second and *may* block forever
 due to a bug in the linux kernel.
 You may want to call `sock.set_read_timeout(Some(Duration::from_secs(1)));` or similar
 prior to calling this function if you require this to actually time out after 1 second.
*/
#[cfg(not(unix))]
pub(crate) fn recv_from(
    batch: &mut RecycledPacketBatch,
    socket: &UdpSocket,
    // If max_wait is None, reads from the socket until either:
    //   * 64 packets are read (PACKETS_PER_BATCH == 64), or
    //   * There are no more data available to read from the socket.
    max_wait: Option<Duration>,
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
    let should_wait = max_wait.is_some();
    let start = should_wait.then(Instant::now);
    loop {
        batch.resize(PACKETS_PER_BATCH, Packet::default());
        match recv_mmsg(socket, &mut batch[i..]) {
            Err(err) if i > 0 => {
                if !should_wait && err.kind() == ErrorKind::WouldBlock {
                    break;
                }
            }
            Err(e) => {
                trace!("recv_from err {e:?}");
                return Err(e);
            }
            Ok(npkts) => {
                if i == 0 {
                    socket.set_nonblocking(true)?;
                }
                trace!("got {npkts} packets");
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
    batch.truncate(i);
    Ok(i)
}

/// Receive multiple messages from `sock` into buffer provided in `batch`.
/// This is a wrapper around recvmmsg(7) call.
#[cfg(unix)]
pub(crate) fn recv_from(
    batch: &mut RecycledPacketBatch,
    socket: &UdpSocket,
    // If max_wait is None, reads from the socket until either:
    //   * 64 packets are read (PACKETS_PER_BATCH == 64), or
    //   * There are no more data available to read from the socket.
    max_wait: Option<Duration>,
    poll_fd: &mut [PollFd],
) -> Result<usize> {
    use crate::streamer::SOCKET_READ_TIMEOUT;

    // Implementation note:
    // This is a reimplementation of the above (now, non-unix) `recv_from` function, and
    // is explicitly meant to preserve the existing behavior, refactored for performance.
    //
    // This implementation is broken into two separate functions:
    // 1. `recv_from_coalesce` - when `max_wait` is provided.
    // 2. `recv_from_once` - when `max_wait` is not provided.
    //
    // This is done to avoid excessive branching in the main loop.

    /// The initial socket polling timeout.
    ///
    /// The socket will be polled for this duration in the event that the initial
    /// `recv_mmsg` call fails with `WouldBlock`.
    ///
    /// This is meant to emulate the blocking behavior of the original `recv_from` function.
    /// The original implementation explicitly sets the socket its given as blocking, and implicitly
    /// expects that the caller will set `socket.set_read_timeout(Some(Duration::from_millis(SOCKET_READ_TIMEOUT)))`
    /// some time before invocation.
    ///
    /// Given that we are using `poll` in this implementation, and we assume the socket is set to
    /// non-blocking, we don't need to worry about `recv_mmsg` hanging indefinitely.
    const SOCKET_READ_TIMEOUT_MS: u16 = SOCKET_READ_TIMEOUT.as_millis() as u16;

    /// Read and batch packets from the socket until batch size is [`PACKETS_PER_BATCH`] or there are no more packets to read.
    ///
    /// Upon calling, this will attempt to read packets from the socket, and poll for [`SOCKET_READ_TIMEOUT`]
    /// when [`ErrorKind::WouldBlock`] is encountered.
    ///
    /// On subsequent iterations, when [`ErrorKind::WouldBlock`] is encountered:
    /// - If any packets were read, the function will exit.
    /// - If no packets were read, the function will return an error.
    fn recv_from_once(
        batch: &mut RecycledPacketBatch,
        socket: &UdpSocket,
        poll_fd: &mut [PollFd],
    ) -> Result<usize> {
        let mut i = 0;
        let mut did_poll = false;

        loop {
            match recv_mmsg(socket, &mut batch[i..]) {
                Ok(npkts) => {
                    i += npkts;
                    if i >= PACKETS_PER_BATCH {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // If we have read any packets, we can exit.
                    if i > 0 {
                        break;
                    }
                    // If we have already polled once, return the error.
                    if did_poll {
                        return Err(e);
                    }
                    did_poll = true;
                    // If we have not read any packets or polled, poll for `SOCKET_READ_TIMEOUT`.
                    if poll(poll_fd, PollTimeout::from(SOCKET_READ_TIMEOUT_MS))? == 0 {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Ok(i)
    }

    /// Read and batch packets from the socket until batch size is [`PACKETS_PER_BATCH`] or `max_wait` is reached.
    ///
    /// Upon calling, this will attempt to read packets from the socket, and poll for [`SOCKET_READ_TIMEOUT`]
    /// when [`ErrorKind::WouldBlock`] is encountered.
    ///
    /// On subsequent iterations, when [`ErrorKind::WouldBlock`] is encountered, poll for the
    /// saturating duration since the start of the loop.
    fn recv_from_coalesce(
        batch: &mut RecycledPacketBatch,
        socket: &UdpSocket,
        max_wait: Duration,
        poll_fd: &mut [PollFd],
    ) -> Result<usize> {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "dragonfly",
            target_os = "freebsd",
        ))]
        const MIN_POLL_DURATION: Duration = Duration::from_micros(100);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "dragonfly",
            target_os = "freebsd",
        )))]
        // `ppoll` is not supported on non-linuxish platforms, so we use `poll`, which only
        // supports millisecond precision.
        const MIN_POLL_DURATION: Duration = Duration::from_millis(1);

        let mut i = 0;
        let deadline = Instant::now() + max_wait;

        loop {
            match recv_mmsg(socket, &mut batch[i..]) {
                Ok(npkts) => {
                    i += npkts;
                    if i >= PACKETS_PER_BATCH {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let timeout = if i == 0 {
                        // This emulates the behavior of the original `recv_from` function,
                        // where it anticipates that the first read of the socket will block for
                        // `crate::streamer::SOCKET_READ_TIMEOUT` before failing with
                        // `ErrorKind::WouldBlock`. The condition `i == 0` indicates that we are just
                        // after the initial read, which did not result in any packets being read.
                        SOCKET_READ_TIMEOUT
                    } else {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        // Avoid excessively short ppoll calls.
                        if remaining < MIN_POLL_DURATION {
                            // Deadline reached.
                            break;
                        }
                        remaining
                    };
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "android",
                        target_os = "dragonfly",
                        target_os = "freebsd",
                    ))]
                    {
                        // Use `ppoll` for its sub-millisecond precision, which ensures that
                        // short coalescing waits (e.g., `max_wait` = 1ms, common in the codebase)
                        // are effective.
                        //
                        // The `poll()` syscall takes an integer millisecond timeout. After a
                        // `recv_mmsg` call, with `max_wait` = 1ms, the remaining wait time is
                        // virtually guaranteed to be a sub-millisecond duration. `poll` would
                        // truncate this remainder to 0ms, preventing any actual polling.
                        // `ppoll` makes coalescing in 1ms windows actually viable.
                        if ppoll(poll_fd, Some(TimeSpec::from_duration(timeout)), None)? == 0 {
                            break;
                        }
                    }
                    #[cfg(not(any(
                        target_os = "linux",
                        target_os = "android",
                        target_os = "dragonfly",
                        target_os = "freebsd",
                    )))]
                    {
                        if poll(poll_fd, PollTimeout::from(timeout.as_millis() as u16))? == 0 {
                            break;
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Ok(i)
    }

    trace!("receiving on {}", socket.local_addr().unwrap());

    let i = match max_wait {
        Some(max_wait) => recv_from_coalesce(batch, socket, max_wait, poll_fd),
        None => recv_from_once(batch, socket, poll_fd),
    }?;

    batch.truncate(i);

    Ok(i)
}
pub fn send_to(
    batch: &RecycledPacketBatch,
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
        super::{recv_from as recv_from_impl, *},
        solana_net_utils::sockets::bind_to_localhost_unique,
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
        let mut packet_batch = RecycledPacketBatch::new(packets);
        packet_batch.set_addr(&send_addr);
        assert_eq!(packet_batch[0].meta().socket_addr(), send_addr);
    }

    fn recv_from(
        batch: &mut RecycledPacketBatch,
        socket: &UdpSocket,
        max_wait: Option<Duration>,
    ) -> Result<usize> {
        #[cfg(unix)]
        {
            use {nix::poll::PollFlags, std::os::fd::AsFd};

            let mut poll_fd = [PollFd::new(socket.as_fd(), PollFlags::POLLIN)];
            recv_from_impl(batch, socket, max_wait, &mut poll_fd)
        }
        #[cfg(not(unix))]
        {
            recv_from_impl(batch, socket, max_wait)
        }
    }

    #[test]
    pub fn packet_send_recv() {
        agave_logger::setup();
        let recv_socket = bind_to_localhost_unique().expect("should bind - receiver");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = bind_to_localhost_unique().expect("should bind - sender");
        let saddr = send_socket.local_addr().unwrap();

        let mut batch = RecycledPacketBatch::with_capacity(PACKETS_PER_BATCH);
        batch.resize(PACKETS_PER_BATCH, Packet::default());

        for m in batch.iter_mut() {
            m.meta_mut().set_socket_addr(&addr);
            m.meta_mut().size = PACKET_DATA_SIZE;
        }
        send_to(&batch, &send_socket, &SocketAddrSpace::Unspecified).unwrap();

        batch
            .iter_mut()
            .for_each(|pkt| *pkt.meta_mut() = Meta::default());
        let recvd = recv_from(
            &mut batch,
            &recv_socket,
            Some(Duration::from_millis(1)), // max_wait
        )
        .unwrap();
        assert_eq!(recvd, batch.len());

        for m in batch.iter() {
            assert_eq!(m.meta().size, PACKET_DATA_SIZE);
            assert_eq!(m.meta().socket_addr(), saddr);
        }
    }

    #[test]
    pub fn debug_trait() {
        write!(io::sink(), "{:?}", Packet::default()).unwrap();
        write!(io::sink(), "{:?}", RecycledPacketBatch::default()).unwrap();
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
        agave_logger::setup();
        let recv_socket = bind_to_localhost_unique().expect("should bind - receiver");
        let addr = recv_socket.local_addr().unwrap();
        let send_socket = bind_to_localhost_unique().expect("should bind - sender");
        let mut batch = RecycledPacketBatch::with_capacity(PACKETS_PER_BATCH);
        batch.resize(PACKETS_PER_BATCH, Packet::default());

        // Should only get PACKETS_PER_BATCH packets per iteration even
        // if a lot more were sent, and regardless of packet size
        for _ in 0..2 * PACKETS_PER_BATCH {
            let batch_size = 1;
            let mut batch = RecycledPacketBatch::with_capacity(batch_size);
            batch.resize(batch_size, Packet::default());
            for p in batch.iter_mut() {
                p.meta_mut().set_socket_addr(&addr);
                p.meta_mut().size = 1;
            }
            send_to(&batch, &send_socket, &SocketAddrSpace::Unspecified).unwrap();
        }
        let recvd = recv_from(
            &mut batch,
            &recv_socket,
            Some(Duration::from_millis(100)), // max_wait
        )
        .unwrap();
        // Check we only got PACKETS_PER_BATCH packets
        assert_eq!(recvd, PACKETS_PER_BATCH);
        assert_eq!(batch.capacity(), PACKETS_PER_BATCH);
    }
}
