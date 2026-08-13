use {
    crate::{
        netlink::MacAddress,
        route::Router,
        umem::{CompletedFrameOffset, Frame, FrameOffset},
    },
    libc::{
        AF_INET, IF_NAMESIZE, SIOCETHTOOL, SIOCGIFADDR, SIOCGIFHWADDR, SOCK_DGRAM, SYS_ioctl,
        ifreq, mmap, munmap, socket, syscall, xdp_ring_offset,
    },
    std::{
        ffi::{CStr, CString, c_char},
        fs,
        io::{self, ErrorKind},
        marker::PhantomData,
        mem,
        net::Ipv4Addr,
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        ptr, slice,
        sync::atomic::{AtomicU32, Ordering},
    },
};

#[derive(Copy, Clone, Debug)]
pub struct QueueId(pub u64);

pub struct NetworkDevice {
    if_index: u32,
    if_name: String,
}

impl NetworkDevice {
    pub fn new(name: impl Into<String>) -> Result<Self, io::Error> {
        let if_name = name.into();
        let if_name_c = CString::new(if_name.as_bytes())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "Invalid interface name"))?;

        let if_index = unsafe { libc::if_nametoindex(if_name_c.as_ptr()) };

        if if_index == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { if_index, if_name })
    }

    pub fn new_from_index(if_index: u32) -> Result<Self, io::Error> {
        let mut buf = [0u8; 1024];
        let ret = unsafe { libc::if_indextoname(if_index, buf.as_mut_ptr() as *mut c_char) };
        if ret.is_null() {
            return Err(io::Error::last_os_error());
        }

        let cstr = unsafe { CStr::from_ptr(ret) };
        let if_name = String::from_utf8_lossy(cstr.to_bytes()).to_string();

        Ok(Self { if_index, if_name })
    }

    pub fn new_from_default_route() -> Result<Self, io::Error> {
        let router = Router::new()?;
        let default_route = router.default().unwrap();
        NetworkDevice::new_from_index(default_route.if_index)
    }

    pub fn name(&self) -> &str {
        &self.if_name
    }

    pub fn if_index(&self) -> u32 {
        self.if_index
    }

    pub fn mac_addr(&self) -> Result<MacAddress, io::Error> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut req: ifreq = unsafe { mem::zeroed() };
        let if_name = CString::new(self.if_name.as_bytes()).unwrap();

        let if_name_bytes = if_name.as_bytes_with_nul();
        let len = std::cmp::min(if_name_bytes.len(), IF_NAMESIZE);
        unsafe {
            std::ptr::copy_nonoverlapping(
                if_name_bytes.as_ptr() as *const c_char,
                req.ifr_name.as_mut_ptr(),
                len,
            );
        }

        let result = unsafe { syscall(SYS_ioctl, fd.as_raw_fd(), SIOCGIFHWADDR, &mut req) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(MacAddress(
            unsafe {
                slice::from_raw_parts(req.ifr_ifru.ifru_hwaddr.sa_data.as_ptr() as *const u8, 6)
            }
            .try_into()
            .unwrap(),
        ))
    }

    pub fn ipv4_addr(&self) -> Result<Ipv4Addr, io::Error> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut req: ifreq = unsafe { mem::zeroed() };
        let if_name = CString::new(self.if_name.as_bytes()).unwrap();

        let if_name_bytes = if_name.as_bytes_with_nul();
        let len = std::cmp::min(if_name_bytes.len(), IF_NAMESIZE);
        unsafe {
            std::ptr::copy_nonoverlapping(
                if_name_bytes.as_ptr() as *const c_char,
                req.ifr_name.as_mut_ptr(),
                len,
            );
        }

        let result = unsafe { syscall(SYS_ioctl, fd.as_raw_fd(), SIOCGIFADDR, &mut req) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        let addr = unsafe {
            let addr_ptr = &req.ifr_ifru.ifru_addr as *const libc::sockaddr;
            let sin_addr = (*(addr_ptr as *const libc::sockaddr_in)).sin_addr;
            Ipv4Addr::from(sin_addr.s_addr.to_ne_bytes())
        };
        Ok(addr)
    }

    pub fn driver(&self) -> io::Result<String> {
        let path = format!("/sys/class/net/{}/device/driver", self.if_name);

        let path = fs::read_link(path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to read driver link for interface {}: {}",
                    self.if_name, e
                ),
            )
        })?;

        Ok(path.file_name().unwrap().to_str().unwrap().into())
    }

    pub fn open_queue(&self, queue_id: QueueId) -> Result<DeviceQueue, io::Error> {
        let ring_sizes = Self::ring_sizes(&self.if_name).ok();
        Ok(DeviceQueue::new(self.if_index, queue_id, ring_sizes))
    }

    pub fn ring_sizes(if_name: &str) -> Result<RingSizes, io::Error> {
        const ETHTOOL_GRINGPARAM: u32 = 0x00000010;

        #[repr(C)]
        struct EthtoolRingParam {
            cmd: u32,
            rx_max_pending: u32,
            rx_mini_max_pending: u32,
            rx_jumbo_max_pending: u32,
            tx_max_pending: u32,
            rx_pending: u32,
            rx_mini_pending: u32,
            rx_jumbo_pending: u32,
            tx_pending: u32,
        }

        let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut rp: EthtoolRingParam = unsafe { mem::zeroed() };
        rp.cmd = ETHTOOL_GRINGPARAM;

        let mut ifr: ifreq = unsafe { mem::zeroed() };
        unsafe {
            ptr::copy_nonoverlapping(
                if_name.as_ptr() as *const c_char,
                ifr.ifr_name.as_mut_ptr(),
                if_name.len().min(IF_NAMESIZE),
            );
        }
        ifr.ifr_name[IF_NAMESIZE - 1] = 0;
        ifr.ifr_ifru.ifru_data = &mut rp as *mut _ as *mut c_char;

        let res = unsafe { syscall(SYS_ioctl, fd.as_raw_fd(), SIOCETHTOOL, &ifr) };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        RingSizes::from_ethtool_pending(if_name, rp.rx_pending, rp.tx_pending)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingSizes {
    pub rx: usize,
    pub tx: usize,
}

impl Default for RingSizes {
    fn default() -> Self {
        // These are reasonable defaults for devices which don't have a set ring size. Values must
        // be a power of two.
        Self { rx: 1024, tx: 1024 }
    }
}

impl RingSizes {
    /// A driver-reported ring size this large would mean multi-gigabyte UMEM allocations;
    /// anything at or above this is treated as an unusable ioctl response rather than a
    /// legitimate configuration. Real NICs top out in the tens of thousands.
    const MAX_RING_SIZE: u32 = 1 << 20;

    /// Validates raw ethtool ring-parameter values before they can reach [`RingSizes`].
    ///
    /// A "successful" `SIOCETHTOOL` ioctl (e.g. on some bonded interfaces) can still report
    /// `rx_pending == 0` or `tx_pending == 0`, or report values that are individually nonzero
    /// but not powers of two, or not power-of-two once combined by callers (see
    /// `xdp::tx_loop::umem_frame_count`). Rejecting anything that isn't a usable nonzero
    /// power of two here means every caller of `ring_sizes()` gets a `None`/`Err` instead of a
    /// value that later panics deep inside `PageAlignedMemory::alloc_with_page_size`'s
    /// `debug_assert!(frame_count.is_power_of_two())` -- the kernel itself rejects non-power-of-two
    /// ring sizes with `EINVAL` at `xsk_init_queue()` bind time, and this crate's own ring index
    /// masking (`index & (size - 1)`) is only correct for powers of two, so a non-power-of-two
    /// value was never actually usable.
    fn from_ethtool_pending(if_name: &str, rx_pending: u32, tx_pending: u32) -> io::Result<Self> {
        let usable = |pending: u32| -> bool {
            pending != 0 && pending.is_power_of_two() && pending < Self::MAX_RING_SIZE
        };
        if !usable(rx_pending) || !usable(tx_pending) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "{if_name} reported an unusable ring size despite a successful ethtool \
                     query (rx={rx_pending}, tx={tx_pending}); expected both to be a nonzero \
                     power of two"
                ),
            ));
        }
        Ok(Self {
            rx: rx_pending as usize,
            tx: tx_pending as usize,
        })
    }
}

pub struct DeviceQueue {
    if_index: u32,
    queue_id: QueueId,
    ring_sizes: Option<RingSizes>,
    completion: Option<TxCompletionRing>,
}

impl DeviceQueue {
    pub fn new(if_index: u32, queue_id: QueueId, ring_sizes: Option<RingSizes>) -> Self {
        Self {
            if_index,
            queue_id,
            ring_sizes,
            completion: None,
        }
    }

    pub fn if_index(&self) -> u32 {
        self.if_index
    }

    pub fn id(&self) -> QueueId {
        self.queue_id
    }

    pub fn tx_completion(&mut self) -> Option<&TxCompletionRing> {
        self.completion.as_ref()
    }

    pub fn ring_sizes(&self) -> Option<RingSizes> {
        self.ring_sizes
    }
}

pub(crate) struct RingConsumer {
    producer: *mut AtomicU32,
    cached_producer: u32,
    consumer: *mut AtomicU32,
    cached_consumer: u32,
}

///Safety: Instances of `RingConsumer` MUST only be resident on one thread at a time
unsafe impl Send for RingConsumer {}

impl RingConsumer {
    pub fn new(producer: *mut AtomicU32, consumer: *mut AtomicU32) -> Self {
        Self {
            producer,
            cached_producer: unsafe { (*producer).load(Ordering::Acquire) },
            consumer,
            cached_consumer: unsafe { (*consumer).load(Ordering::Relaxed) },
        }
    }

    pub fn available(&self) -> u32 {
        self.cached_producer.wrapping_sub(self.cached_consumer)
    }

    pub fn consume(&mut self) -> Option<u32> {
        if self.cached_consumer == self.cached_producer {
            return None;
        }

        let index = self.cached_consumer;
        self.cached_consumer = self.cached_consumer.wrapping_add(1);
        Some(index)
    }

    pub fn commit(&mut self) {
        unsafe { (*self.consumer).store(self.cached_consumer, Ordering::Release) };
    }

    pub fn sync(&mut self, commit: bool) {
        if commit {
            self.commit();
        }
        self.cached_producer = unsafe { (*self.producer).load(Ordering::Acquire) };
    }
}

pub(crate) struct RingProducer {
    producer: *mut AtomicU32,
    cached_producer: u32,
    consumer: *mut AtomicU32,
    cached_consumer: u32,
    size: u32,
}

///Safety: Instances of `RingProducer` MUST only be resident on one thread at a time
unsafe impl Send for RingProducer {}

impl RingProducer {
    pub fn new(producer: *mut AtomicU32, consumer: *mut AtomicU32, size: u32) -> Self {
        Self {
            producer,
            cached_producer: unsafe { (*producer).load(Ordering::Relaxed) },
            consumer,
            cached_consumer: unsafe { (*consumer).load(Ordering::Acquire) },
            size,
        }
    }

    pub fn available(&self) -> u32 {
        self.size
            .saturating_sub(self.cached_producer.wrapping_sub(self.cached_consumer))
    }

    pub fn produce(&mut self) -> Option<u32> {
        if self.available() == 0 {
            return None;
        }

        let index = self.cached_producer;
        self.cached_producer = self.cached_producer.wrapping_add(1);
        Some(index)
    }

    pub fn commit(&mut self) {
        unsafe { (*self.producer).store(self.cached_producer, Ordering::Release) };
    }

    pub fn sync(&mut self, commit: bool) {
        if commit {
            self.commit();
        }
        self.cached_consumer = unsafe { (*self.consumer).load(Ordering::Acquire) };
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub(crate) struct XdpDesc {
    pub(crate) addr: u64,
    pub(crate) len: u32,
    pub(crate) options: u32,
}

pub struct TxCompletionRing {
    mmap: RingMmap<u64>,
    consumer: RingConsumer,
    size: u32,
}

impl TxCompletionRing {
    pub(crate) fn new(mmap: RingMmap<u64>, size: u32) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            consumer: RingConsumer::new(mmap.producer, mmap.consumer),
            mmap,
            size,
        }
    }

    pub fn read(&mut self) -> Option<CompletedFrameOffset> {
        let index = self.consumer.consume()? & self.size.saturating_sub(1);
        let index = unsafe { *self.mmap.desc.add(index as usize) } as usize;
        Some(CompletedFrameOffset(FrameOffset(index)))
    }

    pub fn commit(&mut self) {
        self.consumer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.consumer.sync(commit);
    }
}

pub struct RxFillRing<F: Frame> {
    mmap: RingMmap<u64>,
    producer: RingProducer,
    size: u32,
    _fd: RawFd,
    _frame: PhantomData<F>,
}

impl<F: Frame> RxFillRing<F> {
    pub(crate) fn new(mmap: RingMmap<u64>, size: u32, fd: RawFd) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            producer: RingProducer::new(mmap.producer, mmap.consumer, size),
            mmap,
            size,
            _fd: fd,
            _frame: PhantomData,
        }
    }

    pub fn write(&mut self, frame: F) -> Result<(), io::Error> {
        let Some(index) = self.producer.produce() else {
            return Err(ErrorKind::StorageFull.into());
        };
        let index = index & self.size.saturating_sub(1);
        let desc = unsafe { self.mmap.desc.add(index as usize) };
        // Safety: index is within the ring so the pointer is valid
        unsafe {
            desc.write(frame.offset().0 as u64);
        }

        Ok(())
    }

    pub fn commit(&mut self) {
        self.producer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.producer.sync(commit);
    }
}

pub struct RingMmap<T> {
    pub mmap: *const u8,
    pub mmap_len: usize,
    pub producer: *mut AtomicU32,
    pub consumer: *mut AtomicU32,
    pub desc: *mut T,
    pub flags: *mut AtomicU32,
}

///Safety: Instances of `RingMmap<T>` MUST only be resident on one thread at a time
unsafe impl<T> Send for RingMmap<T> {}

impl<T> Drop for RingMmap<T> {
    fn drop(&mut self) {
        unsafe {
            munmap(self.mmap as *mut _, self.mmap_len);
        }
    }
}

pub(crate) unsafe fn mmap_ring<T>(
    fd: i32,
    size: usize,
    offsets: &xdp_ring_offset,
    ring_type: u64,
) -> Result<RingMmap<T>, io::Error> {
    let map_size = (offsets.desc as usize).saturating_add(size);
    // Safety: just a libc wrapper. We pass a valid size and file descriptor.
    let map_addr = unsafe {
        mmap(
            ptr::null_mut(),
            map_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd,
            ring_type as i64,
        )
    };
    if ptr::eq(map_addr, libc::MAP_FAILED) {
        return Err(io::Error::last_os_error());
    }
    // Safety: manual pointer arithmetic. We are sure that the given offsets
    // don't exceed the bounds.
    unsafe {
        let producer = map_addr.add(offsets.producer as usize) as *mut AtomicU32;
        let consumer = map_addr.add(offsets.consumer as usize) as *mut AtomicU32;
        let desc = map_addr.add(offsets.desc as usize) as *mut T;
        let flags = map_addr.add(offsets.flags as usize) as *mut AtomicU32;
        // V1
        // let flags = map_addr.add(offsets.consumer as usize + mem::size_of::<u32>()) as *mut AtomicU32;
        Ok(RingMmap {
            mmap: map_addr as *const u8,
            mmap_len: map_size,
            producer,
            consumer,
            desc,
            flags,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_ring_sizes_default_is_usable() {
        let default = RingSizes::default();
        assert!(RingSizes::from_ethtool_pending(
            "test0",
            default.rx as u32,
            default.tx as u32
        )
        .is_ok());
    }

    #[test]
    fn test_ring_sizes_from_ethtool_pending_accepts_valid_sizes() {
        // symmetric and legitimately ASYMMETRIC nonzero power-of-two sizes must both
        // be accepted -- individually valid ring sizes are not required to be equal.
        for (rx, tx) in [(1024, 1024), (4096, 512), (512, 4096), (1, 1), (65536, 32768)] {
            assert!(
                RingSizes::from_ethtool_pending("test0", rx, tx).is_ok(),
                "expected rx={rx}, tx={tx} to be accepted"
            );
        }
    }

    #[test]
    fn test_ring_sizes_from_ethtool_pending_rejects_unusable_sizes() {
        // This is the real discriminator for the original issue: a "successful" ioctl
        // (res >= 0) that reports a zero, non-power-of-two, or absurdly large ring size
        // must be rejected here so a bad value never reaches TxLoopBuilder::new.
        for (rx, tx) in [
            (0, 0),
            (0, 1024),
            (1024, 0),
            (1023, 1024),
            (1024, 1023),
            (RingSizes::MAX_RING_SIZE, 1024),
        ] {
            let result = RingSizes::from_ethtool_pending("test0", rx, tx);
            assert!(
                result.is_err(),
                "expected rx={rx}, tx={tx} to be rejected, got {result:?}"
            );
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
        }
    }

    #[test]
    fn test_ring_producer() {
        let mut producer = AtomicU32::new(0);
        let mut consumer = AtomicU32::new(0);
        let size = 16;
        let mut ring = RingProducer::new(&mut producer as *mut _, &mut consumer as *mut _, size);
        assert_eq!(ring.available(), size);

        for i in 0..size {
            assert_eq!(ring.produce(), Some(i));
            assert_eq!(ring.available(), size - i - 1);
        }
        assert_eq!(ring.produce(), None);

        consumer.store(1, Ordering::Release);
        assert_eq!(ring.produce(), None);
        ring.commit();
        assert_eq!(ring.produce(), None);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(16));
        assert_eq!(ring.produce(), None);

        consumer.store(2, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(17));
    }

    #[test]
    fn test_ring_producer_wrap_around() {
        let size = 16;
        let mut producer = AtomicU32::new(u32::MAX - 1);
        let mut consumer = AtomicU32::new(u32::MAX - size - 1);
        let mut ring = RingProducer::new(&mut producer as *mut _, &mut consumer as *mut _, size);
        assert_eq!(ring.available(), 0);

        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(u32::MAX - 1));
        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(u32::MAX));
        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(0));
        consumer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.produce(), Some(1));
    }

    #[test]
    fn test_ring_consumer() {
        let mut producer = AtomicU32::new(0);
        let mut consumer = AtomicU32::new(0);
        let size = 16;
        let mut ring = RingConsumer::new(&mut producer as *mut _, &mut consumer as *mut _);
        assert_eq!(ring.available(), 0);

        producer.store(1, Ordering::Release);
        assert_eq!(ring.available(), 0);
        ring.sync(true);
        assert_eq!(ring.available(), 1);

        producer.store(size, Ordering::Release);
        ring.sync(true);

        for i in 0..size {
            assert_eq!(ring.consume(), Some(i));
            assert_eq!(ring.available(), size - i - 1);
        }
        assert_eq!(ring.consume(), None);
    }

    #[test]
    fn test_ring_consumer_wrap_around() {
        let mut producer = AtomicU32::new(u32::MAX - 1);
        let mut consumer = AtomicU32::new(u32::MAX - 1);
        let mut ring = RingConsumer::new(&mut producer as *mut _, &mut consumer as *mut _);
        assert_eq!(ring.available(), 0);
        assert_eq!(ring.consume(), None);

        producer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.consume(), Some(u32::MAX - 1));

        producer.store(0, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.available(), 1);
        assert_eq!(ring.consume(), Some(u32::MAX));

        producer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.consume(), Some(0));

        producer.fetch_add(1, Ordering::Release);
        ring.sync(true);
        assert_eq!(ring.consume(), Some(1));
    }
}
