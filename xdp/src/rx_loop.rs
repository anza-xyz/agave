#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::{NetworkDevice, QueueId, RingSizes, RxFillRing, XdpDesc},
        set_cpu_affinity,
        socket::{Rx, Socket},
        umem::{FrameOffset, PageAlignedMemory, SliceUmem, SliceUmemFrame, Umem},
    },
    aya::{maps::XskMap, Ebpf},
    bytes::Bytes,
    caps::{
        CapSet,
        Capability::{CAP_NET_ADMIN, CAP_NET_RAW},
    },
    crossbeam_channel::Sender,
    libc::{sysconf, _SC_PAGESIZE},
    std::os::fd::AsFd,
};

const BATCH_SIZE: usize = 512;

pub fn rx_loop(
    cpu_id: usize,
    dev: &NetworkDevice,
    queue_id: QueueId,
    zero_copy: bool,
    sender: Sender<Bytes>,
    bpf_opt: Option<&mut Ebpf>,
) {
    log::info!(
        "starting xdp rx loop on {} queue {queue_id:?} cpu {cpu_id}",
        dev.name()
    );

    set_cpu_affinity([cpu_id]).unwrap();

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

    const HUGE_2MB: usize = 2 * 1024 * 1024;
    let mut memory =
        PageAlignedMemory::alloc_with_page_size(frame_size, frame_count, HUGE_2MB, true)
            .or_else(|_| PageAlignedMemory::alloc(frame_size, frame_count))
            .unwrap();
    let umem = SliceUmem::new(&mut memory, frame_size as u32).unwrap();

    for cap in [CAP_NET_ADMIN, CAP_NET_RAW] {
        caps::raise(None, CapSet::Effective, cap).unwrap();
    }

    let Ok((mut socket, rx)) = Socket::rx(queue, umem, zero_copy, rx_size * 2, rx_size) else {
        panic!("failed to create AF_XDP socket on queue {queue_id:?}");
    };

    // Register socket in xsks_map for XDP_REDIRECT
    if let Some(bpf) = bpf_opt {
        let queue_id = socket.queue().id().0 as u32;
        let fd = socket.as_fd();
        if let Some(map) = bpf.map_mut("xsks_map") {
            if let Ok(mut xsk_map) = XskMap::try_from(map) {
                if let Err(e) = xsk_map.set(queue_id, fd, 0) {
                    log::error!("Failed to set xsks_map[{queue_id}]: {e:?}");
                }
            }
        }
    }

    let umem = socket.umem();

    let Rx {
        fill: mut fill_ring,
        ring: rx_ring,
    } = rx;

    let mut rx_ring = rx_ring.unwrap();

    for cap in [CAP_NET_ADMIN, CAP_NET_RAW] {
        caps::drop(None, CapSet::Effective, cap).unwrap();
    }

    let umem_base = umem.as_ptr();

    let mut descs: [XdpDesc; BATCH_SIZE] = unsafe { std::mem::zeroed() };
    let mut frames: [FrameOffset; BATCH_SIZE] = unsafe { std::mem::zeroed() };

    kick(&fill_ring);

    loop {
        let available = rx_ring.read_batch(&mut descs).unwrap_or(0);

        for (chunk, frame) in descs[..available]
            .chunks_exact(4)
            .zip(frames[..available].chunks_exact_mut(4))
        {
            unsafe {
                let p0 = umem_base.add(chunk[0].addr as usize);
                let p1 = umem_base.add(chunk[1].addr as usize);
                let p2 = umem_base.add(chunk[2].addr as usize);
                let p3 = umem_base.add(chunk[3].addr as usize);

                handle_packet(p0, chunk[0].len as usize, &sender);
                handle_packet(p1, chunk[1].len as usize, &sender);
                handle_packet(p2, chunk[2].len as usize, &sender);
                handle_packet(p3, chunk[3].len as usize, &sender);

                umem.release(FrameOffset(chunk[0].addr as usize));
                umem.release(FrameOffset(chunk[1].addr as usize));
                umem.release(FrameOffset(chunk[2].addr as usize));
                umem.release(FrameOffset(chunk[3].addr as usize));

                frame[0] = FrameOffset(chunk[0].addr as usize);
                frame[1] = FrameOffset(chunk[1].addr as usize);
                frame[2] = FrameOffset(chunk[2].addr as usize);
                frame[3] = FrameOffset(chunk[3].addr as usize);
            }
        }

        let _ = fill_ring.write_batch(umem, &frames);
    }
}

#[inline(always)]
fn kick(ring: &RxFillRing<SliceUmemFrame<'_>>) {
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
        Some(libc::EBUSY | libc::ENOBUFS | libc::EAGAIN) => {}
        Some(libc::ENETDOWN) => {
            log::warn!("network interface is down")
        }
        _ => {
            log::error!("network interface driver error: {e:?}");
        }
    }
}

#[inline(always)]
unsafe fn handle_packet(raw_packet: *const u8, len: usize, sender: &Sender<Bytes>) {
    let byte_slice = unsafe { std::slice::from_raw_parts(raw_packet, len) };
    let bytes = Bytes::copy_from_slice(byte_slice);
    let _ = sender.try_send(bytes);
}
