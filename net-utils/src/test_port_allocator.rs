use {
    crate::sockets::UNIQUE_ALLOC_BASE_PORT,
    std::{
        ffi::{CStr, CString},
        io::Error,
        ops::Range,
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
        unsafe {
            if libc::atexit(at_exit) != 0 {
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

        let region = unsafe {
            // Use a lock file to serialize TestPortAllocator creation.
            let lock_path = lock_file_path(name);

            let lock_fd = libc::open(lock_path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600u32);
            if lock_fd < 0 {
                panic!("open lock file failed: {}", Error::last_os_error());
            }

            // Grab lock before trying to load or create shared memory area.
            if libc::flock(lock_fd, libc::LOCK_EX) != 0 {
                panic!("flock LOCK_EX failed: {}", Error::last_os_error());
            }

            let region = loop {
                let fd = libc::shm_open(
                    name.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    0o600u32,
                );

                if fd >= 0 {
                    // Creator path: size, map, initialize, mark ready.
                    if libc::ftruncate(fd, SHM_SIZE as libc::off_t) != 0 {
                        panic!("ftruncate failed: {}", Error::last_os_error());
                    }
                    let ptr = mmap_shm(fd);
                    libc::close(fd);

                    let region = ptr as *mut SharedRegion;

                    for slot in (*region).ports.iter() {
                        slot.store(-1, Ordering::Relaxed);
                    }

                    (*region).initialized.store(SHM_VERSION, Ordering::Release);

                    break region;
                }

                let errno = Error::last_os_error().raw_os_error().unwrap();
                if errno != libc::EEXIST {
                    panic!("shm_open O_CREAT|O_EXCL failed: errno {errno}");
                }

                // shm already exists; open and check the initialized flag.
                let fd = libc::shm_open(name.as_ptr(), libc::O_RDWR, 0);
                if fd < 0 {
                    let errno = Error::last_os_error().raw_os_error().unwrap();
                    if errno == libc::ENOENT {
                        continue; // raced with an unlink; retry
                    }
                    panic!("shm_open O_RDWR failed: errno {errno}");
                }
                // Check size before mapping to avoid mapping a too-small region.
                // Use < rather than != because MacOS rounds shm sizes up to a
                // platform-specific boundary.
                let mut stat: libc::stat = std::mem::zeroed();
                if libc::fstat(fd, &mut stat) != 0 {
                    panic!("fstat failed: {}", Error::last_os_error());
                }
                if stat.st_size < SHM_SIZE as libc::off_t {
                    // Too small: stale or incompatible segment; clean up and retry.
                    libc::close(fd);
                    libc::shm_unlink(name.as_ptr());
                    continue;
                }

                let ptr = mmap_shm(fd);
                libc::close(fd);

                let region = ptr as *mut SharedRegion;
                if (*region).initialized.load(Ordering::Acquire) == SHM_VERSION {
                    break region;
                }

                // Wrong version or creator crashed mid-init; clean up and retry.
                libc::munmap(ptr, SHM_SIZE);
                libc::shm_unlink(name.as_ptr());
            };

            libc::flock(lock_fd, libc::LOCK_UN);
            libc::close(lock_fd);

            region
        };

        TestPortAllocator { region, tag }
    }

    #[allow(clippy::arithmetic_side_effects)]
    pub fn get_port_range(&self, size: u16) -> Range<u16> {
        assert!(
            size as usize <= PORT_COUNT,
            "requested size {size} exceeds port count {PORT_COUNT}"
        );
        let size = size as usize;
        let pid = unsafe { libc::getpid() };
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
                        let ret = unsafe { libc::kill(owner_pid, 0) };
                        let process_dead =
                            ret == -1 && Error::last_os_error().raw_os_error() == Some(libc::ESRCH);

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
        unsafe {
            libc::shm_unlink(name.as_ptr());
            libc::unlink(lock_file_path(name).as_ptr());
        }
    }

    /// Free all ports allocated by this specific allocator instance (matched by
    /// tag + PID). Must be called on the same instance that performed the
    /// allocations, not a freshly constructed one.
    pub fn cleanup(&self) {
        let pid = unsafe { libc::getpid() };
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
            libc::munmap(self.region as *mut libc::c_void, SHM_SIZE);
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

unsafe fn mmap_shm(fd: i32) -> *mut libc::c_void {
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            SHM_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            panic!("mmap failed: {}", Error::last_os_error());
        }
        ptr
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
