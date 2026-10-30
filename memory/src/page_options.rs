/// Size and alignment of a transparent huge page with 4 KiB base pages, as on x86_64.
pub const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;

pub fn page_size() -> usize {
    #[cfg(unix)]
    {
        static PAGE_SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *PAGE_SIZE.get_or_init(|| {
            // SAFETY: sysconf has no preconditions.
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            usize::try_from(page_size).expect("sysconf(_SC_PAGESIZE) failed")
        })
    }
    #[cfg(not(unix))]
    4096
}

/// Selects whether and how an allocation gets huge pages.
///
/// Huge pages cut page faults and TLB misses in large buffers and tables. The kernel grants
/// transparent huge pages when THP is enabled in `always` or `madvise` mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HugePages {
    /// Allocates regular pages through [`Global`](allocator_api2::alloc::Global).
    Off,
    /// Backs the allocation with transparent huge pages, aligning the edges selected by
    /// [`HugePageAlign`] to huge page boundaries.
    Transparent(HugePageAlign),
    /// Attempts huge pages from the reserved pool (`vm.nr_hugepages`), and falls back to
    /// transparent huge pages when the pool lacks free pages.
    ///
    /// Rounds the allocation up to whole huge pages, on the fallback too. Suits allocations sized
    /// in whole huge pages, or ones that the host reserves a pool for to avoid compaction stalls.
    AttemptReserved,
}

/// Edges of an allocation that lie on huge page boundaries.
///
/// The part of an allocation short of a whole huge page stays on regular pages at an unaligned
/// edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HugePageAlign {
    /// Aligns the start, leaving the regular pages at the end.
    ///
    /// Suits allocations accessed evenly or hottest at their start.
    Start,
    /// Aligns the end, leaving the regular pages at the start.
    ///
    /// Suits allocations whose hot data sits in a small tail after a bulk sized in whole huge
    /// pages, since the regular pages then fall in the less used bulk.
    End,
    /// Aligns both edges by rounding the allocation up to whole huge pages.
    ///
    /// Suits allocations that need huge pages throughout and can spare up to one huge page of
    /// memory each.
    Both,
}

/// Backing that [`PageAllocator`](crate::PageAllocator) and [`AlignedBuffer`](crate::AlignedBuffer)
/// select for their allocations.
///
/// The default backs allocations that span at least one huge page with transparent huge pages,
/// starting at a huge page boundary.
#[derive(Clone, Copy, Debug)]
pub struct AllocOptions {
    pub(crate) huge_pages: HugePages,
    pub(crate) populate: bool,
}

impl AllocOptions {
    pub const fn new() -> Self {
        Self {
            huge_pages: HugePages::Transparent(HugePageAlign::Start),
            populate: false,
        }
    }

    pub const fn huge_pages(mut self, huge_pages: HugePages) -> Self {
        self.huge_pages = huge_pages;
        self
    }

    /// Faults allocations in before returning them, on Linux.
    ///
    /// The page faults and the kernel's zeroing then happen in the allocating thread, instead of in
    /// the threads that first write the memory.
    pub const fn populate(mut self, populate: bool) -> Self {
        self.populate = populate;
        self
    }
}

impl Default for AllocOptions {
    fn default() -> Self {
        Self::new()
    }
}
