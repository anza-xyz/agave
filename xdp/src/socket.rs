use {
    crate::{
        device::{
            DeviceQueue, RingConsumer, RingMmap, RingProducer, RxFillRing, TxCompletionRing,
            XdpDesc, mmap_ring,
        },
        umem::{Frame, Umem},
    },
    libc::{
        AF_XDP, SOCK_RAW, SOL_XDP, XDP_COPY, XDP_MMAP_OFFSETS, XDP_PGOFF_RX_RING,
        XDP_PGOFF_TX_RING, XDP_RING_NEED_WAKEUP, XDP_RX_RING, XDP_TX_RING,
        XDP_UMEM_COMPLETION_RING, XDP_UMEM_FILL_RING, XDP_UMEM_PGOFF_COMPLETION_RING,
        XDP_UMEM_PGOFF_FILL_RING, XDP_USE_NEED_WAKEUP, XDP_ZEROCOPY, bind, getsockopt, sa_family_t,
        sendto, setsockopt, sockaddr, sockaddr_xdp, socket, socklen_t, xdp_mmap_offsets,
        xdp_umem_reg,
    },
    std::{
        io,
        marker::PhantomData,
        mem,
        os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd},
        ptr,
        sync::atomic::Ordering,
    },
};

pub struct Socket<U: Umem> {
    fd: OwnedFd,
    dev_queue: DeviceQueue,
    umem: U,
}

impl<U: Umem> Socket<U> {
    #[allow(clippy::type_complexity)]
    pub fn new(
        dev_queue: DeviceQueue,
        mut umem: U,
        zero_copy: bool,
        rx_fill_ring_size: usize,
        rx_ring_size: usize,
        tx_completion_ring_size: usize,
        tx_ring_size: usize,
    ) -> Result<(Self, Rx<U::Frame>, Tx<U::Frame>), io::Error> {
        unsafe {
            let fd = socket(AF_XDP, SOCK_RAW, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);

            let reg = xdp_umem_reg {
                addr: umem.as_ptr() as u64,
                len: umem.len() as u64,
                chunk_size: umem.frame_size() as u32,
                headroom: 0,
                flags: 0,
                tx_metadata_len: 0,
            };

            if setsockopt(
                fd.as_raw_fd(),
                libc::SOL_XDP,
                libc::XDP_UMEM_REG,
                &reg as *const _ as *const libc::c_void,
                mem::size_of::<xdp_umem_reg>() as libc::socklen_t,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }

            for (ring, size) in [
                (XDP_UMEM_COMPLETION_RING, tx_completion_ring_size),
                (XDP_UMEM_FILL_RING, rx_fill_ring_size),
                (XDP_TX_RING, tx_ring_size),
                (XDP_RX_RING, rx_ring_size),
            ] {
                if ring == XDP_RX_RING && size == 0 {
                    // tx only
                    continue;
                }

                if setsockopt(
                    fd.as_raw_fd(),
                    SOL_XDP,
                    ring,
                    &size as *const _ as *const libc::c_void,
                    mem::size_of::<u32>() as socklen_t,
                ) < 0
                {
                    return Err(io::Error::last_os_error());
                }
            }

            let mut offsets: xdp_mmap_offsets = mem::zeroed();
            let mut optlen = mem::size_of::<xdp_mmap_offsets>() as socklen_t;
            if getsockopt(
                fd.as_raw_fd(),
                SOL_XDP,
                XDP_MMAP_OFFSETS,
                &mut offsets as *mut _ as *mut libc::c_void,
                &mut optlen,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }

            let tx_completion_ring = TxCompletionRing::new(
                mmap_ring(
                    fd.as_raw_fd(),
                    tx_completion_ring_size.saturating_mul(mem::size_of::<u64>()),
                    &offsets.cr,
                    XDP_UMEM_PGOFF_COMPLETION_RING,
                )?,
                tx_completion_ring_size as u32,
            );

            let mut rx_fill_ring = RxFillRing::new(
                mmap_ring(
                    fd.as_raw_fd(),
                    rx_fill_ring_size.saturating_mul(mem::size_of::<u64>()),
                    &offsets.fr,
                    XDP_UMEM_PGOFF_FILL_RING,
                )?,
                rx_fill_ring_size as u32,
                fd.as_raw_fd(),
            );

            if zero_copy {
                // most drivers (intel) are buggy if ZC is enabled and the fill ring is not
                // pre-populated before calling bind()
                for _ in 0..rx_fill_ring_size {
                    let Some(frame) = umem.reserve() else {
                        return Err(io::Error::other("Failed to reserve frame for RX fill ring"));
                    };
                    rx_fill_ring.write(frame)?;
                }
                rx_fill_ring.commit();
            }

            let tx_ring = Some(TxRing::new(
                mmap_ring(
                    fd.as_raw_fd(),
                    tx_ring_size.saturating_mul(mem::size_of::<XdpDesc>()),
                    &offsets.tx,
                    XDP_PGOFF_TX_RING as u64,
                )?,
                tx_ring_size as u32,
                fd.as_raw_fd(),
            ));

            let rx_ring = if rx_ring_size > 0 {
                Some(RxRing::new(
                    mmap_ring(
                        fd.as_raw_fd(),
                        rx_ring_size.saturating_mul(mem::size_of::<XdpDesc>()),
                        &offsets.rx,
                        XDP_PGOFF_RX_RING as u64,
                    )?,
                    rx_ring_size as u32,
                    fd.as_raw_fd(),
                ))
            } else {
                None
            };

            let sxdp = sockaddr_xdp {
                sxdp_family: AF_XDP as sa_family_t,
                // do NEED_WAKEUP and don't do zero copy for now for maximum compatibility
                sxdp_flags: XDP_USE_NEED_WAKEUP | if zero_copy { XDP_ZEROCOPY } else { XDP_COPY },
                sxdp_ifindex: dev_queue.if_index(),
                sxdp_queue_id: dev_queue.id().0 as u32,
                sxdp_shared_umem_fd: 0,
            };

            if bind(
                fd.as_raw_fd(),
                &sxdp as *const _ as *const sockaddr,
                mem::size_of::<sockaddr_xdp>() as socklen_t,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }

            let tx = Tx {
                completion: tx_completion_ring,
                ring: tx_ring,
            };
            let rx = Rx {
                fill: rx_fill_ring,
                ring: rx_ring,
            };
            Ok((
                Self {
                    fd,
                    dev_queue,
                    umem,
                },
                rx,
                tx,
            ))
        }
    }

    pub fn tx(
        queue: DeviceQueue,
        umem: U,
        zero_copy: bool,
        completion_size: usize,
        ring_size: usize,
    ) -> Result<(Self, Tx<U::Frame>), io::Error> {
        let (fill_size, rx_size) = if zero_copy {
            // See Socket::new() as to why this is needed
            let rx = queue
                .ring_sizes()
                .ok_or_else(|| io::Error::other("zero copy requires a set ring size"))?
                .rx;
            (rx, rx)
        } else {
            // no RX fill ring needed for TX only sockets
            (1, 0)
        };
        let (socket, _, tx) = Self::new(
            queue,
            umem,
            zero_copy,
            fill_size,
            rx_size,
            completion_size,
            ring_size,
        )?;
        Ok((socket, tx))
    }

    pub fn rx(
        queue: DeviceQueue,
        umem: U,
        zero_copy: bool,
        fill_size: usize,
        ring_size: usize,
    ) -> Result<(Self, Rx<U::Frame>), io::Error> {
        let (socket, rx, _) = Self::new(queue, umem, zero_copy, fill_size, ring_size, 0, 0)?;
        Ok((socket, rx))
    }

    pub fn queue(&self) -> &DeviceQueue {
        &self.dev_queue
    }

    pub fn umem(&mut self) -> &mut U {
        &mut self.umem
    }
}

impl<U: Umem> AsFd for Socket<U> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

pub struct Tx<F: Frame> {
    pub completion: TxCompletionRing,
    pub ring: Option<TxRing<F>>,
}

pub struct Rx<F: Frame> {
    pub fill: RxFillRing<F>,
    pub ring: Option<RxRing>,
}

pub struct TxRing<F: Frame> {
    mmap: RingMmap<XdpDesc>,
    producer: RingProducer,
    size: u32,
    fd: RawFd,
    _frame: PhantomData<F>,
}

#[derive(Debug)]
pub struct RingFull<F: Frame>(pub F);

impl<F: Frame> TxRing<F> {
    fn new(mmap: RingMmap<XdpDesc>, size: u32, fd: RawFd) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            producer: RingProducer::new(mmap.producer, mmap.consumer, size),
            mmap,
            size,
            fd,
            _frame: PhantomData,
        }
    }

    pub fn write(&mut self, frame: F, options: u32) -> Result<(), RingFull<F>> {
        let Some(index) = self.producer.produce() else {
            return Err(RingFull(frame));
        };
        let index = index & self.size.saturating_sub(1);
        unsafe {
            let desc = self.mmap.desc.add(index as usize);
            desc.write(XdpDesc {
                addr: frame.offset().0 as u64,
                len: frame.len() as u32,
                options,
            });
        }
        Ok(())
    }

    pub fn needs_wakeup(&self) -> bool {
        unsafe { (*self.mmap.flags).load(Ordering::Relaxed) & XDP_RING_NEED_WAKEUP != 0 }
    }

    pub fn wake(&self) -> Result<u64, io::Error> {
        let result = unsafe { sendto(self.fd, ptr::null(), 0, libc::MSG_DONTWAIT, ptr::null(), 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result as u64)
    }

    pub fn capacity(&self) -> usize {
        self.size as usize
    }

    pub fn available(&self) -> usize {
        self.producer.available() as usize
    }

    pub fn commit(&mut self) {
        self.producer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.producer.sync(commit);
    }
}

pub struct RxRing {
    #[allow(dead_code)]
    mmap: RingMmap<XdpDesc>,
    consumer: RingConsumer,
    size: u32,
    #[allow(dead_code)]
    fd: RawFd,
}

impl RxRing {
    fn new(mmap: RingMmap<XdpDesc>, size: u32, fd: RawFd) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            consumer: RingConsumer::new(mmap.producer, mmap.consumer),
            mmap,
            size,
            fd,
        }
    }

    pub fn capacity(&self) -> usize {
        self.size as usize
    }

    pub fn available(&self) -> usize {
        self.consumer.available() as usize
    }

    pub fn commit(&mut self) {
        self.consumer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.consumer.sync(commit);
    }
}
