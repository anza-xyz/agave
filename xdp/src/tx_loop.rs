#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::{DeviceQueue, NetworkDevice, QueueId, RingSizes, TxCompletionRing},
        gre::{construct_gre_packet, gre_packet_size},
        netlink::MacAddress,
        packet::{
            ETH_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE, write_eth_header,
            write_ip_header_for_udp, write_udp_header,
        },
        route::NextHop,
        set_cpu_affinity,
        socket::{Socket, Tx, TxRing},
        umem::{Frame, OwnedUmem, PageAlignedMemory, Umem},
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError},
    libc::{_SC_PAGESIZE, sysconf},
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        thread,
        time::Duration,
    },
};

pub struct TxLoopConfigBuilder {
    zero_copy: bool,
    maybe_src_mac: Option<MacAddress>,
    maybe_src_ip: Option<Ipv4Addr>,
    src_port: u16,
}

impl TxLoopConfigBuilder {
    pub fn new(src_port: u16) -> Self {
        Self {
            zero_copy: false,
            maybe_src_mac: None,
            maybe_src_ip: None,
            src_port,
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

    pub fn override_src_ip(&mut self, ip: Ipv4Addr) -> &mut Self {
        self.maybe_src_ip = Some(ip);
        self
    }

    pub fn build_with_src_device(self, src_device: &NetworkDevice) -> TxLoopConfig {
        let Self {
            zero_copy,
            maybe_src_mac,
            maybe_src_ip,
            src_port,
        } = self;

        let src_mac = maybe_src_mac.unwrap_or_else(|| {
            // if no source MAC is provided, use the device's MAC address
            src_device
                .mac_addr()
                .expect("no src_mac provided, device must have a MAC address")
        });

        let src_ip = maybe_src_ip.unwrap_or_else(|| {
            // if no source IP is provided, use the device's IPv4 address
            src_device
                .ipv4_addr()
                .expect("no src_ip provided, device must have an IPv4 address")
        });

        TxLoopConfig {
            zero_copy,
            src_mac,
            src_ip,
            src_port,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TxLoopConfig {
    zero_copy: bool,
    src_mac: MacAddress,
    src_ip: Ipv4Addr,
    src_port: u16,
}

pub struct TxLoopBuilder<U: Umem> {
    cpu_id: usize,
    queue_id: QueueId,
    zero_copy: bool,
    src_mac: MacAddress,
    src_ip: Ipv4Addr,
    src_port: u16,
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
        let TxLoopConfig {
            zero_copy,
            src_mac,
            src_ip,
            src_port,
        } = config;

        log::info!(
            "starting xdp loop on {} queue {queue_id:?} cpu {cpu_id}",
            dev.name()
        );

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
            queue_id,
            zero_copy,
            src_mac,
            src_ip,
            src_port,
            queue,
            tx_size,
            umem,
        }
    }

    pub fn build(self) -> TxLoop<OwnedUmem<PageAlignedMemory>> {
        let TxLoopBuilder {
            cpu_id,
            queue_id,
            zero_copy,
            src_mac,
            src_ip,
            src_port,
            queue,
            tx_size,
            umem,
        } = self;

        let Ok((socket, tx)) = Socket::tx(queue, umem, zero_copy, tx_size * 2, tx_size) else {
            panic!("failed to create AF_XDP socket on queue {queue_id:?}");
        };

        let Tx {
            // this is where we'll queue frames
            ring,
            // this is where we'll get completion events once frames have been picked up by the NIC
            completion,
        } = tx;
        let ring = ring.unwrap();

        TxLoop {
            cpu_id,
            src_mac,
            src_ip,
            src_port,
            socket,
            ring,
            completion,
        }
    }
}

pub struct TxLoop<U: Umem> {
    cpu_id: usize,
    src_mac: MacAddress,
    src_ip: Ipv4Addr,
    src_port: u16,
    socket: Socket<U>,
    ring: TxRing<U::Frame>,
    completion: TxCompletionRing,
}

impl<U: Umem> TxLoop<U> {
    pub fn run<T: AsRef<[u8]>, A: AsRef<[SocketAddr]>, R: Fn(&IpAddr) -> Option<NextHop>>(
        self,
        receiver: Receiver<(A, T)>,
        drop_sender: Sender<(A, T)>,
        route_fn: R,
    ) {
        // How long we sleep waiting to receive shreds from the channel.
        const RECV_TIMEOUT: Duration = Duration::from_nanos(1000);

        const MAX_TIMEOUTS: usize = 1;

        // We try to collect _at least_ BATCH_SIZE packets before queueing into the NIC. This is to
        // avoid introducing too much per-packet overhead and giving the NIC time to complete work
        // before we queue the next chunk of packets.
        const BATCH_SIZE: usize = 64;

        let TxLoop {
            cpu_id,
            src_mac,
            src_ip,
            src_port,
            mut socket,
            mut ring,
            mut completion,
        } = self;

        // each queue is bound to its own CPU core
        set_cpu_affinity([cpu_id]).unwrap();

        let umem = socket.umem();
        let umem_tx_capacity = umem.available();

        // Local buffer where we store packets before sending themi.
        let mut batched_items = Vec::with_capacity(BATCH_SIZE);

        // How many packets we've batched. This is _not_ batched_items.len(), but item * peers. For
        // example if we have 3 packets to transmit to 2 destination addresses each, we have 6 batched
        // packets.
        let mut batched_packets = 0;

        let mut timeouts = 0;
        loop {
            match receiver.try_recv() {
                Ok((addrs, payload)) => {
                    batched_packets += addrs.as_ref().len();
                    batched_items.push((addrs, payload));
                    timeouts = 0;
                    if batched_packets < BATCH_SIZE {
                        continue;
                    }
                }
                Err(TryRecvError::Empty) => {
                    if timeouts < MAX_TIMEOUTS {
                        timeouts += 1;
                        thread::sleep(RECV_TIMEOUT);
                    } else {
                        timeouts = 0;
                        // we haven't received anything in a while, kick the driver
                        ring.commit();
                        kick(&ring);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    // keep looping until we've flushed all the packets
                    if batched_packets == 0 {
                        break;
                    }
                }
            };

            // this is the number of packets after which we commit the ring and kick the driver if
            // necessary
            let mut chunk_remaining = BATCH_SIZE.min(batched_packets);

            for (addrs, payload) in batched_items.drain(..) {
                for addr in addrs.as_ref() {
                    if ring.available() == 0 || umem.available() == 0 {
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

                    let len = payload.as_ref().len();

                    let dst = addr.ip();
                    let Some(next_hop) = route_fn(&dst) else {
                        log::warn!("dropping packet: no route for peer {addr}");
                        batched_packets -= 1;
                        umem.release(frame.offset());
                        continue;
                    };

                    if let Some(gre) = &next_hop.gre {
                        frame.set_len(gre_packet_size(len));
                        let packet = umem.map_frame_mut(&frame);
                        let inner_src_ip = next_hop.preferred_src_ip.unwrap_or(src_ip);
                        if let Err(err) = construct_gre_packet(
                            packet,
                            &src_mac,
                            &gre.mac_addr,
                            &inner_src_ip,
                            &dst_ip,
                            src_port,
                            addr.port(),
                            payload.as_ref(),
                            &gre.tunnel_info,
                        ) {
                            log::warn!("dropping packet: {err}");
                            batched_packets -= 1;
                            umem.release(frame.offset());
                            continue;
                        }
                    } else {
                        // we need the MAC address to send the packet
                        let Some(dest_mac) = next_hop.mac_addr else {
                            log::warn!(
                                "dropping packet: peer {addr} must be routed through {} which has \
                                 no known MAC address",
                                next_hop.ip_addr
                            );
                            batched_packets -= 1;
                            umem.release(frame.offset());
                            continue;
                        };

                        const PACKET_HEADER_SIZE: usize =
                            ETH_HEADER_SIZE + IP_HEADER_SIZE + UDP_HEADER_SIZE;
                        frame.set_len(PACKET_HEADER_SIZE + len);
                        let packet = umem.map_frame_mut(&frame);

                        // write the payload first as it's needed for checksum calculation (if enabled)
                        packet[PACKET_HEADER_SIZE..][..len].copy_from_slice(payload.as_ref());

                        write_eth_header(packet, &src_mac.0, &dest_mac.0);

                        write_ip_header_for_udp(
                            &mut packet[ETH_HEADER_SIZE..],
                            &src_ip,
                            &dst_ip,
                            (UDP_HEADER_SIZE + len) as u16,
                        );

                        write_udp_header(
                            &mut packet[ETH_HEADER_SIZE + IP_HEADER_SIZE..],
                            &src_ip,
                            src_port,
                            &dst_ip,
                            addr.port(),
                            len as u16,
                            // don't do checksums
                            false,
                        );
                    }

                    ring.write(frame, 0)
                        .map_err(|_| "ring full")
                        // this should never happen as we check for available slots above
                        .expect("failed to write to ring");

                    batched_packets -= 1;
                    chunk_remaining -= 1;

                    // check if it's time to commit the ring and kick the driver
                    if chunk_remaining == 0 {
                        chunk_remaining = BATCH_SIZE.min(batched_packets);

                        // commit new frames
                        ring.commit();
                        kick(&ring);
                    }
                }
                let _ = drop_sender.try_send((addrs, payload));
            }
            debug_assert_eq!(batched_packets, 0);
        }
        assert_eq!(batched_packets, 0);

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
