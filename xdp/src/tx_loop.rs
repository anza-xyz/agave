#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::{NetworkDevice, QueueId, RingSizes},
        netlink::InterfaceInfo,
        netlink::MacAddress,
        packet::{
            construct_gre_packet, write_eth_header, write_ip_header_for_udp, write_udp_header,
            ETH_HEADER_SIZE, GRE_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE,
        },
        route::NextHop,
        set_cpu_affinity,
        socket::{Socket, Tx, TxRing},
        umem::{Frame as _, PageAlignedMemory, SliceUmem, SliceUmemFrame, Umem as _},
    },
    caps::{
        CapSet,
        Capability::{CAP_NET_ADMIN, CAP_NET_RAW},
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError},
    libc::{sysconf, _SC_PAGESIZE},
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        thread,
        time::Duration,
    },
};

#[allow(clippy::too_many_arguments)]
pub fn tx_loop<
    T: AsRef<[u8]>,
    A: AsRef<[SocketAddr]>,
    R: Fn(&IpAddr) -> Option<(NextHop, InterfaceInfo)>,
>(
    cpu_id: usize,
    dev: &NetworkDevice,
    queue_id: QueueId,
    zero_copy: bool,
    src_mac: Option<MacAddress>,
    src_ip: Option<Ipv4Addr>,
    src_port: u16,
    receiver: Receiver<(A, T)>,
    drop_sender: Sender<(A, T)>,
    route_fn: R,
) {
    log::info!(
        "starting xdp loop on {} queue {queue_id:?} cpu {cpu_id}",
        dev.name()
    );

    // each queue is bound to its own CPU core
    set_cpu_affinity([cpu_id]).unwrap();

    let src_mac = src_mac.unwrap_or_else(|| {
        // if no source MAC is provided, use the device's MAC address
        dev.mac_addr()
            .expect("no src_mac provided, device must have a MAC address")
    });

    let src_ip = src_ip.unwrap_or_else(|| {
        // if no source IP is provided, use the device's IPv4 address
        dev.ipv4_addr()
            .expect("no src_ip provided, device must have an IPv4 address")
    });
    log::info!("greg: xdp: using src ip address {src_ip}");

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
    let mut memory =
        PageAlignedMemory::alloc_with_page_size(frame_size, frame_count, HUGE_2MB, true)
            .or_else(|_| {
                log::warn!("huge page alloc failed, falling back to regular page size");
                PageAlignedMemory::alloc(frame_size, frame_count)
            })
            .unwrap();
    let umem = SliceUmem::new(&mut memory, frame_size as u32).unwrap();

    // we need NET_ADMIN and NET_RAW for the socket
    for cap in [CAP_NET_ADMIN, CAP_NET_RAW] {
        caps::raise(None, CapSet::Effective, cap).unwrap();
    }

    let Ok((mut socket, tx)) = Socket::tx(queue, umem, zero_copy, tx_size * 2, tx_size) else {
        panic!("failed to create AF_XDP socket on queue {queue_id:?}");
    };

    let umem = socket.umem();
    let umem_tx_capacity = umem.available();
    let Tx {
        // this is where we'll queue frames
        ring,
        // this is where we'll get completion events once frames have been picked up by the NIC
        mut completion,
    } = tx;
    let mut ring = ring.unwrap();

    // we don't need higher caps anymore
    for cap in [CAP_NET_ADMIN, CAP_NET_RAW] {
        caps::drop(None, CapSet::Effective, cap).unwrap();
    }

    // How long we sleep waiting to receive shreds from the channel.
    const RECV_TIMEOUT: Duration = Duration::from_nanos(1000);

    const MAX_TIMEOUTS: usize = 1;

    // We try to collect _at least_ BATCH_SIZE packets before queueing into the NIC. This is to
    // avoid introducing too much per-packet overhead and giving the NIC time to complete work
    // before we queue the next chunk of packets.
    const BATCH_SIZE: usize = 64;

    // Local buffer where we store packets before sending themi.
    let mut batched_items = Vec::with_capacity(BATCH_SIZE);

    // How many packets we've batched. This is _not_ batched_items.len(), but item * peers. For
    // example if we have 3 packets to transmit to 2 destination addresses each, we have 6 batched
    // packets.
    let mut batched_packets = 0;

    //greg: todo implement gre caching
    // Cache the underlay MAC for the current router epoch and GRE remote.
    // (Fast-path: single GRE remote per queue. If you have multiple, switch to a small HashMap.)
    // let mut cached_underlay: Option<(
    //     usize,    /*router update counter*/
    //     Ipv4Addr, /*gre.remote*/
    //     MacAddress,
    // )> = None;

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

        // greg: todo implement update_counter for gre caching
        // let update_counter = atomic_router.update_counter();
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
                let dest_mac = {
                    let ip = addr.ip();
                    let Some((next_hop, interface_info)) = route_fn(&ip) else {
                        log::warn!("dropping packet: no route for peer {addr}");
                        batched_packets -= 1;
                        umem.release(frame.offset());
                        continue;
                    };

                    // we need the MAC address to send the packet
                    let Some(dest_mac) = next_hop.mac_addr else {
                        log::warn!(
                            "dropping packet: peer {addr} must be routed through {} which has no \
                                known MAC address",
                            next_hop.ip_addr
                        );
                        batched_packets -= 1;
                        umem.release(frame.offset());
                        continue;
                    };

                    // log::info!("greg: xdp: dst_ip nh: {next_hop:?} iface: {interface_info:?}");
                    if let Some(gre) = interface_info.gre_tunnel.as_ref() {
                        // greg: todo implement underlay MAC caching
                        let outer_dst_mac = dest_mac;
                        // Resolve underlay MAC with tiny cache keyed by epoch and gre remote
                        // let outer_dst_mac = match cached_underlay.as_ref() {
                        //     Some((uc, ip, mac)) if *uc == update_counter && *ip == gre.remote => {
                        //         *mac
                        //     }
                        //     _ => {
                        //         let (nh, _iface) = router.route(IpAddr::V4(gre.remote)).unwrap();
                        //         // log::info!("greg: xdp: new gre n nh: {nh:?} iface: {iface:?}");
                        //         match nh.mac_addr {
                        //             Some(m) => {
                        //                 cached_underlay = Some((update_counter, gre.remote, m));
                        //                 // log::info!(
                        //                 //     "greg: xdp: cached underlay MAC: {m}, remote: {}",
                        //                 //     gre.remote
                        //                 // );
                        //                 m
                        //             }
                        //             None => {
                        //                 // log::warn!(
                        //                 //     "greg: dropping GRE pkt: missing underlay MAC for next-hop {} on {}({})",
                        //                 //     nh.ip_addr, iface.if_name, iface.if_index
                        //                 // );
                        //                 batched_packets -= 1;
                        //                 umem.release(frame.offset());
                        //                 continue;
                        //             }
                        //         }
                        //     }
                        // };

                        // Calculate GRE packet size
                        const INNER_PACKET_HEADER_SIZE: usize = IP_HEADER_SIZE + UDP_HEADER_SIZE;
                        let inner_packet_len = INNER_PACKET_HEADER_SIZE + len;
                        let gre_packet_size =
                            ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_SIZE + inner_packet_len;

                        // Reserve space for GRE packet
                        frame.set_len(gre_packet_size);
                        let packet = umem.map_frame_mut(&frame);

                        let inner_src_ip = next_hop.preferred_src_ip.unwrap_or(src_ip);
                        // log::info!("greg: xdp: inner src ip: {inner_src_ip}");

                        // Construct the GRE packet
                        let gre_packet_len = construct_gre_packet(
                            packet,
                            &inner_src_ip, // inner src ip
                            &dst_ip,       // inner dst ip
                            src_port,
                            addr.port(),
                            payload.as_ref(),
                            gre.local,        // gre src ip
                            gre.remote,       // gre dst ip
                            &src_mac.0,       // src MAC (our nic)
                            &outer_dst_mac.0, //outer dst MAC (underlay next-hop)
                        );

                        // Update frame length and submit packet
                        frame.set_len(gre_packet_len);
                        submit_packet_to_ring(
                            frame,
                            &mut ring,
                            &mut batched_packets,
                            &mut chunk_remaining,
                            BATCH_SIZE,
                        );

                        continue;
                    }
                    dest_mac
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

                // write the packet into the ring
                submit_packet_to_ring(
                    frame,
                    &mut ring,
                    &mut batched_packets,
                    &mut chunk_remaining,
                    BATCH_SIZE,
                );
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

// With some drivers, or always when we work in SKB mode, we need to explicitly kick the driver once
// we want the NIC to do something.
#[inline(always)]
fn kick(ring: &TxRing<SliceUmemFrame<'_>>) {
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

/// Common packet submission logic - handles ring writing, batching, and driver kicking
fn submit_packet_to_ring<'a>(
    frame: SliceUmemFrame<'a>,
    ring: &mut TxRing<SliceUmemFrame<'a>>,
    batched_packets: &mut usize,
    chunk_remaining: &mut usize,
    batch_size: usize,
) {
    // Write the packet into the ring
    ring.write(frame, 0)
        .map_err(|_| "ring full")
        .expect("failed to write to ring");

    *batched_packets -= 1;
    *chunk_remaining -= 1;

    // Check if it's time to commit the ring and kick the driver
    if *chunk_remaining == 0 {
        *chunk_remaining = batch_size.min(*batched_packets);
        ring.commit();
        kick(ring);
    }
}
