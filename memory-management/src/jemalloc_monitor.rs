#![cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
use {
    jemallocator::Jemalloc,
    std::{
        alloc::{GlobalAlloc, Layout},
        cell::RefCell,
        sync::{
            atomic::{AtomicUsize, Ordering},
            RwLock,
        },
    },
};

const NAME_LEN: usize = 16;
type ThreadName = arrayvec::ArrayVec<u8, NAME_LEN>;

static SELF: JemWrapStats = JemWrapStats {
    named_thread_stats: RwLock::new(None),
    unnamed_thread_stats: Counters::new(),
    process_stats: Counters::new(),
};

#[derive(Debug)]
pub struct Counters {
    allocations_total: AtomicUsize,
    deallocations_total: AtomicUsize,
    bytes_allocated_total: AtomicUsize,
    bytes_deallocated_total: AtomicUsize,
}

pub struct CountersView {
    pub allocations_total: usize,
    pub deallocations_total: usize,
    pub bytes_allocated_total: usize,
    pub bytes_deallocated_total: usize,
}

impl Default for Counters {
    fn default() -> Self {
        Self::new()
    }
}

impl Counters {
    pub fn view(&self) -> CountersView {
        CountersView {
            allocations_total: self.allocations_total.load(Ordering::Relaxed),
            deallocations_total: self.deallocations_total.load(Ordering::Relaxed),
            bytes_allocated_total: self.bytes_allocated_total.load(Ordering::Relaxed),
            bytes_deallocated_total: self.bytes_deallocated_total.load(Ordering::Relaxed),
        }
    }

    const fn new() -> Self {
        Self {
            allocations_total: AtomicUsize::new(0),
            deallocations_total: AtomicUsize::new(0),
            bytes_allocated_total: AtomicUsize::new(0),
            bytes_deallocated_total: AtomicUsize::new(0),
        }
    }
}

impl Counters {
    pub fn alloc(&self, size: usize) {
        self.bytes_allocated_total
            .fetch_add(size, Ordering::Relaxed);
        self.allocations_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dealloc(&self, size: usize) {
        self.bytes_deallocated_total
            .fetch_add(size, Ordering::Relaxed);
        self.deallocations_total.fetch_add(1, Ordering::Relaxed);
    }
}

#[repr(C, align(4096))]
pub struct JemWrapAllocator {
    jemalloc: Jemalloc,
}

impl JemWrapAllocator {
    pub const fn new() -> Self {
        Self { jemalloc: Jemalloc }
    }
}

impl Default for JemWrapAllocator {
    fn default() -> Self {
        Self::new()
    }
}

struct JemWrapStats {
    pub named_thread_stats: RwLock<Option<MemPoolStats>>,
    pub unnamed_thread_stats: Counters,
    pub process_stats: Counters,
}

pub fn view_allocations(f: impl FnOnce(&MemPoolStats)) {
    let lock_guard = &SELF.named_thread_stats.read().unwrap();
    if let Some(stats) = lock_guard.as_ref() {
        f(stats);
    }
}
pub fn view_global_allocations() -> (CountersView, CountersView) {
    (SELF.unnamed_thread_stats.view(), SELF.process_stats.view())
}

#[derive(Debug, Default)]
pub struct MemPoolStats {
    pub data: Vec<(ThreadName, Counters)>,
}

impl MemPoolStats {
    pub fn add(&mut self, prefix: &str) {
        let key: ThreadName = prefix
            .as_bytes()
            .try_into()
            .unwrap_or_else(|_| panic!("Prefix can not be over {} bytes long", NAME_LEN));

        self.data.push((key, Counters::default()));
        // keep data sorted with longest prefixes first (this avoids short-circuiting)
        // no need for this to be efficient since we do not run time in a tight loop and vec is typically short
        self.data.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()));
    }
}

pub fn init_allocator(mps: MemPoolStats) {
    SELF.named_thread_stats.write().unwrap().replace(mps);
}

pub fn deinit_allocator() -> MemPoolStats {
    SELF.named_thread_stats.write().unwrap().take().unwrap()
}

unsafe impl Sync for JemWrapAllocator {}

unsafe impl GlobalAlloc for JemWrapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let alloc = self.jemalloc.alloc(layout);
        if alloc.is_null() {
            return alloc;
        }
        SELF.process_stats.alloc(layout.size());
        if let Ok(stats) = SELF.named_thread_stats.try_read() {
            if let Some(stats) = stats.as_ref() {
                if let Some(stats) = match_thread_name_safely(stats, true) {
                    stats.alloc(layout.size());
                }
            }
        } else {
            SELF.unnamed_thread_stats.alloc(layout.size());
        }
        alloc
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.jemalloc.dealloc(ptr, layout);
        if ptr.is_null() {
            return;
        }
        SELF.process_stats.dealloc(layout.size());
        if let Ok(stats) = SELF.named_thread_stats.try_read() {
            if let Some(stats) = stats.as_ref() {
                if let Some(stats) = match_thread_name_safely(stats, false) {
                    stats.dealloc(layout.size());
                }
            }
        } else {
            SELF.unnamed_thread_stats.dealloc(layout.size());
        }
    }
}

thread_local! (
    static THREAD_NAME: RefCell<ThreadName> = RefCell::new(ThreadName::new())
);

fn match_thread_name_safely(stats: &MemPoolStats, insert_if_missing: bool) -> Option<&Counters> {
    let name: Option<ThreadName> = THREAD_NAME
        .try_with(|v| {
            let mut name = v.borrow_mut();
            if name.is_empty() {
                if insert_if_missing {
                    unsafe {
                        name.set_len(NAME_LEN);
                        let res = libc::pthread_getname_np(
                            libc::pthread_self(),
                            name.as_mut_ptr() as *mut i8,
                            name.capacity(),
                        );
                        if res == 0 {
                            let name_len = memchr::memchr(0, &name).unwrap_or(name.len());
                            name.set_len(name_len);
                        }
                    }
                } else {
                    return None;
                }
            }
            Some(name.clone())
        })
        .ok()
        .flatten();
    match name {
        Some(name) => {
            for (prefix, stats) in stats.data.iter() {
                if !name.starts_with(prefix) {
                    continue;
                }
                return Some(stats);
            }
            None
        }
        None => None,
    }
}
