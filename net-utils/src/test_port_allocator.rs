use {
    crate::sockets::UNIQUE_ALLOC_BASE_PORT,
    nix::{
        errno::Errno,
        fcntl::{Flock, FlockArg, OFlag, open},
        sys::{
            mman::{MapFlags, ProtFlags, mmap, munmap, shm_open, shm_unlink},
            signal::kill,
            stat::{Mode, fstat},
        },
        unistd::{Pid, ftruncate, getpid},
    },
    std::{
        ffi::{CStr, CString, c_void},
        num::NonZeroUsize,
        ops::Range,
        os::fd::AsFd,
        ptr::NonNull,
        sync::{
            Mutex, OnceLock,
            atomic::{AtomicI64, Ordering},
        },
    },
};

/// On first call, a [`TestPortAllocator`] backed by POSIX shared memory is
/// instantiated and an `atexit` handler is registered to release the ports
/// when the process exits. This makes allocation safe across concurrent
/// nextest invocations and multiple git worktrees on the same host.
pub fn unique_port_range_for_tests_internal(size: u16) -> Range<u16> {
    use crate::test_port_allocator::TestPortAllocator;

    static ALLOCATOR: OnceLock<TestPortAllocator> = OnceLock::new();
    const SHM_NAME: &CStr = c"/shared_port_allocator";

    let allocator = ALLOCATOR.get_or_init(|| {
        extern "C" fn at_exit() {
            if let Some(alloc) = ALLOCATOR.get() {
                alloc.cleanup();
            }
        }
        unsafe extern "C" {
            fn atexit(f: extern "C" fn()) -> std::ffi::c_int;
        }
        unsafe {
            if atexit(at_exit) != 0 {
                eprintln!(
                    "warning: failed to register atexit handler for TestPortAllocator; ports may \
                     not be cleaned up on process exit"
                );
            }
        }
        TestPortAllocator::new(SHM_NAME)
    });

    allocator.get_port_range(size)
}

static CREATION_MUTEX: Mutex<()> = Mutex::new(());

const PORT_MIN: u16 = UNIQUE_ALLOC_BASE_PORT;
const PORT_MAX: u16 = 65534; // 65535 excluded: range end would overflow u16
const PORT_COUNT: usize = (PORT_MAX - PORT_MIN + 1) as usize;
const SHM_SIZE: usize = std::mem::size_of::<SharedRegion>();
const SHM_VERSION: i64 = 2; // bump whenever SharedRegion layout changes
const GET_PORT_MAX_RETRIES: u32 = 10000;
const GET_PORT_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);

// Entry encoding (i64):
//   -1            : free slot
//   bit 63 = 0    : occupied
//   bits 62-32    : random 31-bit tag (guards against PID-recycle ABA)
//   bits 31-0     : owning PID
#[repr(C)]
struct SharedRegion {
    initialized: AtomicI64, // 0 = not ready, SHM_VERSION = fully initialized
    ports: [AtomicI64; PORT_COUNT], // -1 = free, otherwise encoded tag+PID
}

pub struct TestPortAllocator {
    region: *mut SharedRegion,
    tag: i64, // pre-shifted 31-bit random tag: (rand_31bit as i64) << 32
}

unsafe impl Send for TestPortAllocator {}
unsafe impl Sync for TestPortAllocator {}

impl TestPortAllocator {
    pub fn new(name: &CStr) -> Self {
        let tag = (rand::random::<u32>() as i64 & 0x7FFF_FFFF) << 32;

        // Serialize within-process creation: on MacOS (BSD), flock() is per-process
        // so it does not block other threads in the same process. This mutex fills
        // that gap while the flock handles cross-process serialization.
        let _creation_guard = CREATION_MUTEX.lock().unwrap();

        let lock_path = lock_file_path(name);
        let lock_fd = open(
            lock_path.as_c_str(),
            OFlag::O_CREAT | OFlag::O_RDWR,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .expect("open lock file failed");

        // Grab lock before trying to load or create shared memory area.
        let locked_fd = Flock::lock(lock_fd, FlockArg::LockExclusive)
            .unwrap_or_else(|(_, e)| panic!("flock LOCK_EX failed: {e}"));

        let region = loop {
            match shm_open(
                name,
                OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR,
                Mode::S_IRUSR | Mode::S_IWUSR,
            ) {
                Ok(fd) => {
                    // Creator path: size, map, initialize, mark ready.
                    ftruncate(fd.as_fd(), SHM_SIZE as _).expect("ftruncate failed");
                    let ptr = unsafe { mmap_shm(fd.as_fd()) };
                    drop(fd);

                    let region = ptr.as_ptr() as *mut SharedRegion;
                    unsafe {
                        for slot in (*region).ports.iter() {
                            slot.store(-1, Ordering::Relaxed);
                        }
                        (*region).initialized.store(SHM_VERSION, Ordering::Release);
                    }
                    break region;
                }
                Err(Errno::EEXIST) => {
                    // shm already exists; open and check the initialized flag.
                    match shm_open(name, OFlag::O_RDWR, Mode::empty()) {
                        Err(Errno::ENOENT) => continue, // raced with an unlink; retry
                        Err(e) => panic!("shm_open O_RDWR failed: {e}"),
                        Ok(fd) => {
                            // Check size before mapping to avoid mapping a too-small region.
                            // Use < rather than != because MacOS rounds shm sizes up to a
                            // platform-specific boundary.
                            let stat = fstat(fd.as_fd()).expect("fstat failed");
                            if stat.st_size < SHM_SIZE as _ {
                                // Too small: stale or incompatible segment; clean up and retry.
                                drop(fd);
                                let _ = shm_unlink(name);
                                continue;
                            }

                            let ptr = unsafe { mmap_shm(fd.as_fd()) };
                            drop(fd);

                            let region = ptr.as_ptr() as *mut SharedRegion;
                            if unsafe { (*region).initialized.load(Ordering::Acquire) }
                                == SHM_VERSION
                            {
                                break region;
                            }

                            // Wrong version or creator crashed mid-init; clean up and retry.
                            unsafe {
                                let _ = munmap(ptr, SHM_SIZE);
                            }
                            let _ = shm_unlink(name);
                        }
                    }
                }
                Err(e) => panic!("shm_open O_CREAT|O_EXCL failed: {e}"),
            }
        };

        drop(locked_fd); // unlocks and closes the lock file

        TestPortAllocator { region, tag }
    }

    #[allow(clippy::arithmetic_side_effects)]
    pub fn get_port_range(&self, size: u16) -> Range<u16> {
        assert!(
            size as usize <= PORT_COUNT,
            "requested size {size} exceeds port count {PORT_COUNT}"
        );
        let size = size as usize;
        let pid = getpid().as_raw();
        let marker = encode(self.tag, pid);
        let ports = unsafe { &(*self.region).ports };

        let mut retries = 0u32;

        'outer: loop {
            let start = random_start(size);
            let mut claimed = 0usize;

            // Try to find size consecutive free ports beginning with start.
            for i in 0..size {
                'inner: loop {
                    let current = ports[start + i].load(Ordering::Acquire);

                    if current == -1 {
                        match ports[start + i].compare_exchange(
                            -1,
                            marker,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break 'inner,     // claimed
                            Err(_) => continue 'inner, // raced; retry
                        }
                    } else {
                        // Extract PID from lower 32 bits and check liveness.
                        let owner_pid = (current & 0xFFFF_FFFF) as i32;
                        let process_dead =
                            kill(Pid::from_raw(owner_pid), None) == Err(Errno::ESRCH);

                        if process_dead {
                            // Tag+PID together guard against ABA: a new process
                            // with the same PID will have a different tag.
                            let _ = ports[start + i].compare_exchange(
                                current,
                                -1,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );
                            continue 'inner;
                        } else {
                            // Port is held by a live process; roll back and try a new start.
                            for j in 0..claimed {
                                ports[start + j].store(-1, Ordering::Release);
                            }
                            retries += 1;
                            if retries >= GET_PORT_MAX_RETRIES {
                                panic!(
                                    "get_port_range: no free {size}-port range after \
                                     {GET_PORT_MAX_RETRIES} retries"
                                );
                            }
                            std::thread::sleep(GET_PORT_RETRY_SLEEP);
                            continue 'outer;
                        }
                    }
                }
                claimed += 1;
            }

            let port_start = PORT_MIN + start as u16;
            return port_start..(port_start + size as u16);
        }
    }

    #[cfg(test)]
    pub fn destroy(name: &CStr) {
        let _ = shm_unlink(name);
        let _ = nix::unistd::unlink(lock_file_path(name).as_c_str());
    }

    /// Free all ports allocated by this specific allocator instance (matched by
    /// tag + PID). Must be called on the same instance that performed the
    /// allocations, not a freshly constructed one.
    pub fn cleanup(&self) {
        let pid = getpid().as_raw();
        let marker = encode(self.tag, pid);
        let ports = unsafe { &(*self.region).ports };
        for slot in ports.iter() {
            let _ = slot.compare_exchange(marker, -1, Ordering::AcqRel, Ordering::Relaxed);
        }
    }
}

impl Drop for TestPortAllocator {
    fn drop(&mut self) {
        unsafe {
            let _ = munmap(NonNull::new_unchecked(self.region as *mut c_void), SHM_SIZE);
        }
    }
}

/// Encode a tag+PID pair into a slot value.
/// bit 63 = 0 (always non-negative), bits 62-32 = tag, bits 31-0 = PID.
fn encode(tag: i64, pid: i32) -> i64 {
    tag | (pid as i64 & 0xFFFF_FFFF)
}

#[allow(clippy::arithmetic_side_effects)]
fn random_start(size: usize) -> usize {
    use rand::Rng;
    rand::rng().random_range(0..=(PORT_COUNT - size))
}

/// Derives the init lock file path from the shm name.
/// e.g. "/foo_bar" → "/tmp/foo_bar_init.lock"
fn lock_file_path(name: &CStr) -> CString {
    let name_str = name.to_str().expect("shm name must be valid UTF-8");
    CString::new(format!(
        "/tmp/{}_init.lock",
        name_str.trim_start_matches('/')
    ))
    .unwrap()
}

unsafe fn mmap_shm(fd: impl AsFd) -> NonNull<c_void> {
    unsafe {
        mmap(
            None,
            NonZeroUsize::new(SHM_SIZE).unwrap(),
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            fd,
            0,
        )
        .expect("mmap failed")
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{
            collections::HashSet,
            sync::{Arc, Barrier},
        },
    };

    #[test]
    fn test_no_duplicate_ports() {
        const THREADS: usize = 100;
        const ALLOCS_PER_THREAD: usize = 100;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let shm_name = Arc::new(CString::new(format!("/portalloc_test_{nanos}")).unwrap());

        let start_barrier = Arc::new(Barrier::new(THREADS));
        let create_barrier = Arc::new(Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let shm_name = Arc::clone(&shm_name);
                let start_barrier = Arc::clone(&start_barrier);
                let create_barrier = Arc::clone(&create_barrier);
                std::thread::spawn(move || {
                    start_barrier.wait();
                    let alloc = TestPortAllocator::new(shm_name.as_c_str());
                    create_barrier.wait();

                    let mut ports = Vec::with_capacity(ALLOCS_PER_THREAD);
                    for _ in 0..ALLOCS_PER_THREAD {
                        let range = alloc.get_port_range(1);
                        ports.push(range.start);
                    }
                    (alloc, ports)
                })
            })
            .collect();

        let results: Vec<(TestPortAllocator, Vec<u16>)> = handles
            .into_iter()
            .map(|h| h.join().expect("thread panicked"))
            .collect();

        // All allocations are done; now clean up each instance with its own tag.
        for (alloc, _) in &results {
            alloc.cleanup();
        }

        let all_ports: Vec<u16> = results.into_iter().flat_map(|(_, ports)| ports).collect();

        assert_eq!(
            all_ports.len(),
            THREADS * ALLOCS_PER_THREAD,
            "expected {} total allocations",
            THREADS * ALLOCS_PER_THREAD
        );

        let unique: HashSet<u16> = all_ports.iter().copied().collect();
        assert_eq!(
            unique.len(),
            all_ports.len(),
            "duplicate ports detected across threads"
        );

        TestPortAllocator::destroy(shm_name.as_c_str());
    }
}
