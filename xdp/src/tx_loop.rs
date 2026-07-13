#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::{DeviceQueue, NetworkDevice, QueueId, RingSizes, TxCompletionRing, TxRing},
        ecn_codepoint::EcnCodepoint,
        gre::{
            construct_gre_packet, gre_packet_size,
            packet::{GRE_HEADER_BASE_SIZE, INNER_PACKET_HEADER_SIZE},
        },
        netlink::MacAddress,
        packet::{
            IP_HEADER_SIZE, PACKET_HEADER_SIZE, UDP_HEADER_SIZE, VLAN_PACKET_HEADER_SIZE,
            construct_packet, construct_vlan_packet,
        },
        route::NextHop,
        socket::{Socket, Tx},
        umem::{Frame, OwnedUmem, PageAlignedMemory, Umem},
    },
    agave_cpu_utils::set_cpu_affinity,
    crossbeam_queue::ArrayQueue,
    libc::{_SC_PAGESIZE, sysconf},
    std::{
        error::Error,
        fmt, io,
        net::{IpAddr, SocketAddr, SocketAddrV4},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    },
};

pub struct TxLoopConfigBuilder {
    zero_copy: bool,
    maybe_src_mac: Option<MacAddress>,
}

impl TxLoopConfigBuilder {
    pub fn new() -> Self {
        Self {
            zero_copy: false,
            maybe_src_mac: None,
        }
    }

    pub fn zero_copy(&mut self, enable: bool) -> &mut Self {
        self.zero_copy = enable;
        self
    }

    pub fn override_src_mac(&mut self, mac: MacAddress) -> &mut Self {
        self.maybe_src_mac = Some(mac);
        self
    }

    pub fn build_with_src_device(self, src_device: &NetworkDevice) -> TxLoopConfig {
        let Self {
            zero_copy,
            maybe_src_mac,
        } = self;

        let src_mac = maybe_src_mac.unwrap_or_else(|| {
            // if no source MAC is provided, use the device's MAC address
            src_device
                .mac_addr()
                .expect("no src_mac provided, device must have a MAC address")
        });

        TxLoopConfig { zero_copy, src_mac }
    }
}

impl Default for TxLoopConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct TxLoopConfig {
    zero_copy: bool,
    src_mac: MacAddress,
}

pub struct TxLoopBuilder<U: Umem> {
    cpu_id: usize,
    zero_copy: bool,
    src_mac: MacAddress,
    queue: DeviceQueue,
    tx_size: usize,
    umem: U,
}

impl TxLoopBuilder<OwnedUmem<PageAlignedMemory>> {
    pub fn new(
        cpu_id: usize,
        queue_id: QueueId,
        config: TxLoopConfig,
        dev: &NetworkDevice,
    ) -> TxLoopBuilder<OwnedUmem<PageAlignedMemory>> {
        let TxLoopConfig { zero_copy, src_mac } = config;

        log::info!(
            "starting xdp loop on {} queue {queue_id:?} cpu {cpu_id}",
            dev.name()
        );

        // We don't support MTUs larger than page size due to AF_XDP limitations in single-buffer
        // mode and a possible workaround might be to use multi-buffer TX.

        // some drivers require frame_size=page_size
        let frame_size = unsafe { sysconf(_SC_PAGESIZE) } as usize;

        let queue = dev
            .open_queue(queue_id)
            .expect("failed to open queue for AF_XDP socket");
        let RingSizes {
            rx: rx_size,
            tx: tx_size,
        } = queue.ring_sizes().unwrap_or_else(|| {
            log::info!(
                "using default ring sizes for {} queue {queue_id:?}",
                dev.name()
            );
            RingSizes::default()
        });

        let frame_count = (rx_size + tx_size) * 2;

        // try to allocate huge pages first, then fall back to regular pages
        const HUGE_2MB: usize = 2 * 1024 * 1024;
        let memory =
            PageAlignedMemory::alloc_with_page_size(frame_size, frame_count, HUGE_2MB, true)
                .or_else(|_| {
                    log::warn!("huge page alloc failed, falling back to regular page size");
                    PageAlignedMemory::alloc(frame_size, frame_count)
                })
                .unwrap();
        let umem = OwnedUmem::new(memory, frame_size as u32).unwrap();

        TxLoopBuilder {
            cpu_id,
            zero_copy,
            src_mac,
            queue,
            tx_size,
            umem,
        }
    }

    pub fn build(self) -> Result<TxLoop<OwnedUmem<PageAlignedMemory>>, io::Error> {
        let TxLoopBuilder {
            cpu_id,
            zero_copy,
            src_mac,
            queue,
            tx_size,
            umem,
        } = self;

        let queue_id = queue.id();
        let (socket, tx) =
            Socket::tx(queue, umem, zero_copy, tx_size * 2, tx_size).map_err(|err| {
                log::error!(
                    "failed to create AF_XDP TX socket for queue {queue_id:?} on CPU {cpu_id}: \
                     {err}"
                );
                err
            })?;

        let Tx {
            // this is where we'll queue frames
            ring,
            // this is where we'll get completion events once frames have been picked up by the NIC
            completion,
        } = tx;
        let ring = ring.unwrap();

        Ok(TxLoop {
            cpu_id,
            src_mac,
            socket,
            ring,
            completion,
        })
    }
}

pub struct TxLoop<U: Umem> {
    cpu_id: usize,
    src_mac: MacAddress,
    socket: Socket<U>,
    ring: TxRing<U::Frame>,
    completion: TxCompletionRing,
}

/// [`TxPacket`] represents a packet to transmit via XDP to a list of addresses with the provided
/// `payload` and source address.
pub trait TxPacket {
    type Addrs: AsRef<[SocketAddr]>;
    type Payload: AsRef<[u8]>;

    /// List of destination addresses to which the packet should be sent.
    fn dst_addrs(&self) -> &Self::Addrs;

    /// Payload of the packet to be sent.
    fn payload(&self) -> &Self::Payload;

    /// Source address used when sending the packet.
    fn src_addr(&self) -> SocketAddrV4;

    /// Explicit congestion notification bits to set on the packet.
    fn ecn(&self) -> Option<EcnCodepoint>;

    /// Returns true when this packet is expected to occasionally exceed the route MTU.
    fn allow_mtu_overflow(&self) -> bool;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("Full(..)"),
            Self::Disconnected(_) => f.write_str("Disconnected(..)"),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("sending into a full XDP transmit channel"),
            Self::Disconnected(_) => f.write_str("sending into a closed XDP transmit channel"),
        }
    }
}

impl<T> Error for TrySendError<T> {}

pub trait Receiver<T> {
    fn try_recv(&self) -> Result<T, TryRecvError>;
}

pub struct TxSender<T> {
    queue: Arc<SharedQueue<T>>,
}

pub struct TxReceiver<T> {
    queue: Arc<SharedQueue<T>>,
}

struct SharedQueue<T> {
    queue: ArrayQueue<T>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
}

pub fn channel<T>(capacity: usize) -> (TxSender<T>, TxReceiver<T>) {
    let queue = Arc::new(SharedQueue {
        queue: ArrayQueue::new(capacity),
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
    });
    (
        TxSender {
            queue: Arc::clone(&queue),
        },
        TxReceiver { queue },
    )
}

impl<T> Clone for TxSender<T> {
    fn clone(&self) -> Self {
        self.queue.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}

impl<T> Drop for TxSender<T> {
    fn drop(&mut self) {
        self.queue.senders.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<T> TxSender<T> {
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        if self.queue.receivers.load(Ordering::Relaxed) == 0 {
            return Err(TrySendError::Disconnected(item));
        }
        self.queue.queue.push(item).map_err(TrySendError::Full)
    }
}

impl<T> Drop for TxReceiver<T> {
    fn drop(&mut self) {
        self.queue.receivers.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<T> Receiver<T> for TxReceiver<T> {
    fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.queue.queue.pop() {
            Some(item) => Ok(item),
            // there is a potential race here if the last sender sends an item and then immediately
            // drops. In that case we'll return Disconnected even though there's an item queued.
            // That's fine since it's only a shutdown race and we don't guarantee packet delivery in
            // any case.
            None if self.queue.senders.load(Ordering::Relaxed) == 0 => {
                Err(TryRecvError::Disconnected)
            }
            None => Err(TryRecvError::Empty),
        }
    }
}

impl<U: Umem> TxLoop<U> {
    pub fn run<T, Rx, D, R>(self, receiver: Rx, mut drop_item: D, route_fn: R)
    where
        T: TxPacket,
        Rx: Receiver<T>,
        D: FnMut(T),
        R: Fn(&IpAddr) -> Option<NextHop>,
    {
        // How long we sleep waiting to receive packets from the channel.
        const RECV_TIMEOUT: Duration = Duration::from_nanos(1000);

        const MAX_TIMEOUTS: usize = 1;

        // Publish TX descriptors in batches to avoid per-packet commits and driver kicks. An idle
        // timeout commits any partial batch so low rate traffic is not held indefinitely.
        const BATCH_SIZE: usize = 64;

        let TxLoop {
            cpu_id,
            src_mac,
            mut socket,
            mut ring,
            mut completion,
        } = self;

        // each queue is bound to its own CPU core
        set_cpu_affinity(None, [agave_cpu_utils::CpuId::new(cpu_id).unwrap()]).unwrap();

        let umem = socket.umem();
        let umem_tx_capacity = umem.available();
        let umem_frame_size = umem.frame_size();

        // How many descriptors are written into the TX ring but not yet committed.
        let mut written_uncommitted = 0;

        let mut timeouts = 0;
        loop {
            let item = match receiver.try_recv() {
                Ok(item) => {
                    timeouts = 0;
                    item
                }
                Err(TryRecvError::Empty) => {
                    if timeouts < MAX_TIMEOUTS {
                        timeouts += 1;
                        thread::sleep(RECV_TIMEOUT);
                    } else {
                        timeouts = 0;
                        commit_pending(&mut ring, &mut written_uncommitted);
                        // we haven't received anything in a while, kick the driver
                        kick(&ring);
                    }
                    continue;
                }
                Err(TryRecvError::Disconnected) => break,
            };

            let src_addr = item.src_addr();
            let src_ip = src_addr.ip();
            let src_port = src_addr.port();
            let ecn = item.ecn();
            let can_overflow_mtu = item.allow_mtu_overflow();
            for addr in item.dst_addrs().as_ref() {
                if ring.available() == 0 || umem.available() == 0 {
                    commit_pending(&mut ring, &mut written_uncommitted);
                    kick(&ring);

                    // loop until we have space for the next packet
                    loop {
                        completion.sync(true);
                        // we haven't written any frames so we only need to sync the consumer position
                        ring.sync(false);

                        // check if any frames were completed
                        while let Some(frame_offset) = completion.read() {
                            umem.release(frame_offset);
                        }

                        if ring.available() > 0 && umem.available() > 0 {
                            // we have space for the next packet, break out of the loop
                            break;
                        }

                        // queues are full, if NEEDS_WAKEUP is set kick the driver so hopefully it'll
                        // complete some work
                        kick(&ring);
                    }
                }

                // at this point we're guaranteed to have a frame to write the next packet into and
                // a slot in the ring to submit it
                let mut frame = umem.reserve().unwrap();
                let IpAddr::V4(dst_ip) = addr.ip() else {
                    panic!("IPv6 not supported");
                };

                let payload = item.payload().as_ref();
                let len = payload.len();

                let dst = addr.ip();
                let Some(next_hop) = route_fn(&dst) else {
                    log::warn!("dropping packet: no route for peer {addr}");
                    umem.release(frame.offset());
                    continue;
                };

                if let Some(gre) = &next_hop.gre {
                    let l3_inner_packet_len = INNER_PACKET_HEADER_SIZE + len;
                    let l3_outer_gre_packet_len =
                        IP_HEADER_SIZE + GRE_HEADER_BASE_SIZE + l3_inner_packet_len;

                    if l3_inner_packet_len > gre.mtu as usize
                        || l3_outer_gre_packet_len > next_hop.mtu as usize
                    {
                        if !can_overflow_mtu {
                            log::warn!(
                                "dropping packet: GRE payload exceeds MTU for {addr}: L3 inner \
                                 packet length {l3_inner_packet_len}, L3 outer GRE packet length \
                                 {l3_outer_gre_packet_len}, MTU: {mtu}, underlay_mtu: \
                                 {underlay_mtu}.",
                                mtu = gre.mtu,
                                underlay_mtu = next_hop.mtu
                            );
                        }
                        umem.release(frame.offset());
                        continue;
                    }

                    let packet_len = gre_packet_size(len);
                    if packet_len > umem_frame_size {
                        log::warn!(
                            "dropping packet: GRE packet size {packet_len} exceeds frame size \
                             {umem_frame_size} for {addr}"
                        );
                        umem.release(frame.offset());
                        continue;
                    }

                    frame.set_len(packet_len);
                    let packet = umem.map_frame_mut(&frame);
                    let inner_src_ip = next_hop.preferred_src_ip.as_ref().unwrap_or(src_ip);
                    if let Err(err) = construct_gre_packet(
                        packet,
                        &src_mac,
                        &gre.mac_addr,
                        inner_src_ip,
                        &dst_ip,
                        src_port,
                        addr.port(),
                        payload,
                        ecn,
                        &gre.tunnel_info,
                    ) {
                        log::warn!("dropping packet: {err}");
                        umem.release(frame.offset());
                        continue;
                    }
                } else if let Some(vlan) = &next_hop.vlan {
                    // we need the MAC address to send the packet
                    let Some(dest_mac) = next_hop.mac_addr else {
                        log::warn!(
                            "dropping packet: peer {addr} must be routed through {} which has no \
                             known MAC address",
                            next_hop.ip_addr
                        );
                        umem.release(frame.offset());
                        continue;
                    };

                    // The 802.1Q tag is added at L2, so the L3 size compared against the MTU
                    // is the same as the untagged path.
                    let l3_packet_len = IP_HEADER_SIZE + UDP_HEADER_SIZE + len;
                    if l3_packet_len > next_hop.mtu as usize {
                        if !can_overflow_mtu {
                            log::warn!(
                                "dropping packet: packet size {l3_packet_len} exceeds MTU {mtu} \
                                 for {addr}",
                                mtu = next_hop.mtu
                            );
                        }
                        umem.release(frame.offset());
                        continue;
                    }

                    let packet_len = VLAN_PACKET_HEADER_SIZE + len;
                    if packet_len > umem_frame_size {
                        log::warn!(
                            "dropping packet: VLAN packet size {packet_len} exceeds frame size \
                             {umem_frame_size} for {addr}"
                        );
                        umem.release(frame.offset());
                        continue;
                    }

                    frame.set_len(packet_len);
                    let packet = umem.map_frame_mut(&frame);

                    // The route's preferred src is the IP assigned to the VLAN sub-interface,
                    // which is the right inner src for traffic egressing this VLAN. Fall back
                    // to the device's src IP if the route did not carry one.
                    let inner_src_ip = next_hop.preferred_src_ip.as_ref().unwrap_or(src_ip);

                    if !construct_vlan_packet(
                        packet,
                        &src_mac.0,
                        &dest_mac.0,
                        inner_src_ip,
                        &dst_ip,
                        src_port,
                        addr.port(),
                        vlan.vid,
                        vlan.pcp,
                        payload,
                        ecn,
                    ) {
                        log::warn!("dropping packet: VLAN frame did not fit in UMEM slot");
                        umem.release(frame.offset());
                        continue;
                    }
                } else {
                    // we need the MAC address to send the packet
                    let Some(dest_mac) = next_hop.mac_addr else {
                        log::warn!(
                            "dropping packet: peer {addr} must be routed through {} which has no \
                             known MAC address",
                            next_hop.ip_addr
                        );
                        umem.release(frame.offset());
                        continue;
                    };

                    let l3_packet_len = IP_HEADER_SIZE + UDP_HEADER_SIZE + len;
                    if l3_packet_len > next_hop.mtu as usize {
                        if !can_overflow_mtu {
                            log::warn!(
                                "dropping packet: packet size {l3_packet_len} exceeds MTU {mtu} \
                                 for {addr}",
                                mtu = next_hop.mtu
                            );
                        }
                        umem.release(frame.offset());
                        continue;
                    }

                    let packet_len = PACKET_HEADER_SIZE + len;
                    if packet_len > umem_frame_size {
                        log::warn!(
                            "dropping packet: packet size {packet_len} exceeds frame size \
                             {umem_frame_size} for {addr}"
                        );
                        umem.release(frame.offset());
                        continue;
                    }

                    frame.set_len(packet_len);
                    let packet = umem.map_frame_mut(&frame);

                    if !construct_packet(
                        packet,
                        &src_mac.0,
                        &dest_mac.0,
                        src_ip,
                        &dst_ip,
                        src_port,
                        addr.port(),
                        payload,
                        ecn,
                    ) {
                        log::warn!("dropping packet: frame did not fit in UMEM slot");
                        umem.release(frame.offset());
                        continue;
                    }
                }

                ring.write(frame, 0)
                    .map_err(|_| "ring full")
                    // this should never happen as we check for available slots above
                    .expect("failed to write to ring");

                written_uncommitted += 1;

                // check if it's time to publish descriptors and kick the driver
                if written_uncommitted >= BATCH_SIZE {
                    commit_pending(&mut ring, &mut written_uncommitted);
                    kick(&ring);
                }
            }
            drop_item(item);
        }
        commit_pending(&mut ring, &mut written_uncommitted);
        kick(&ring);

        // drain the ring
        while umem.available() < umem_tx_capacity || ring.available() < ring.capacity() {
            log::debug!(
                "draining xdp ring umem {}/{} ring {}/{}",
                umem.available(),
                umem_tx_capacity,
                ring.available(),
                ring.capacity()
            );

            completion.sync(true);
            while let Some(frame_offset) = completion.read() {
                umem.release(frame_offset);
            }

            ring.sync(false);
            kick(&ring);
        }
    }
}

#[inline(always)]
fn commit_pending<F: Frame>(ring: &mut TxRing<F>, pending_uncommitted: &mut usize) {
    if *pending_uncommitted == 0 {
        return;
    }
    ring.commit();
    *pending_uncommitted = 0;
}

// With some drivers, or always when we work in SKB mode, we need to explicitly kick the driver once
// we want the NIC to do something.
#[inline(always)]
fn kick<F: Frame>(ring: &TxRing<F>) {
    if !ring.needs_wakeup() {
        return;
    }

    if let Err(e) = ring.wake() {
        kick_error(e);
    }
}

#[inline(never)]
fn kick_error(e: std::io::Error) {
    match e.raw_os_error() {
        // these are non-fatal errors
        Some(libc::EBUSY | libc::ENOBUFS | libc::EAGAIN) => {}
        // this can temporarily happen with some drivers when changing
        // settings (eg with ethtool)
        Some(libc::ENETDOWN) => {
            log::warn!("network interface is down")
        }
        // we should never get here, hopefully the driver recovers?
        _ => {
            log::error!("network interface driver error: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tx_loop::{Receiver, TryRecvError, TrySendError, channel};

    #[test]
    fn test_send_full() {
        let (sender, _receiver) = channel(1);

        assert!(sender.try_send(1).is_ok());
        match sender.try_send(2) {
            Err(TrySendError::Full(item)) => assert_eq!(item, 2),
            result => panic!("expected full queue, got {result:?}"),
        }
    }

    #[test]
    fn test_send_disconnected() {
        let (sender, receiver) = channel(1);
        drop(receiver);

        match sender.try_send(1) {
            Err(TrySendError::Disconnected(item)) => assert_eq!(item, 1),
            result => panic!("expected disconnected queue, got {result:?}"),
        }
    }

    #[test]
    fn test_recv_disconnected() {
        let (sender, receiver) = channel(1);
        sender
            .try_send(1)
            .expect("send item before dropping sender");
        drop(sender);

        assert_eq!(receiver.try_recv().expect("receive queued item"), 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn test_recv_waits_for_cloned_sender() {
        let (sender, receiver) = channel::<i32>(1);
        let sender_clone = sender.clone();
        drop(sender);

        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        drop(sender_clone);
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }
}
