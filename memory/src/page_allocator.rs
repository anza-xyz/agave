use {
    crate::{
        AllocOptions, HUGE_PAGE_SIZE, HugePageAlign, HugePages,
        mmap::{map, map_reserved, populate, unmap},
    },
    allocator_api2::alloc::{AllocError, Allocator, Global, Layout},
    std::ptr::NonNull,
};

/// Allocator backing allocations with huge pages or regular pages.
///
/// It allocates directly from the kernel when [`AllocOptions`] selects huge page backing for an
/// allocation, which by default covers allocations spanning at least one huge page. Everything else
/// comes from [`Global`].
#[derive(Clone, Copy, Debug, Default)]
pub struct PageAllocator {
    options: AllocOptions,
}

impl PageAllocator {
    pub const fn new(options: AllocOptions) -> Self {
        Self { options }
    }

    fn huge_page_mode(&self, layout: Layout) -> Option<HugePageMode> {
        if !cfg!(target_os = "linux") || layout.size() == 0 {
            return None;
        }
        let fits_huge_page = layout.size() >= HUGE_PAGE_SIZE;
        match self.options.huge_pages {
            HugePages::Off => None,
            HugePages::Transparent(HugePageAlign::Both) => Some(HugePageMode::Transparent(
                HugePageAlign::Both,
                round_up_to_huge_pages(layout)?,
            )),
            HugePages::Transparent(aligned_edges) => {
                fits_huge_page.then_some(HugePageMode::Transparent(aligned_edges, layout))
            }
            // The fallback rounds too, so `deallocate` unmaps the same length without knowing
            // which path `allocate` took.
            HugePages::AttemptReserved => Some(HugePageMode::AttemptReserved(
                round_up_to_huge_pages(layout)?,
            )),
        }
    }

    fn allocate_reserved_or_transparent(layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match map_reserved(layout) {
            Ok(ptr) => Ok(NonNull::slice_from_raw_parts(ptr, layout.size())),
            Err(error) => {
                log::debug!(
                    "reserved huge pages unavailable for {} bytes: {error}",
                    layout.size()
                );
                Self::allocate_transparent(layout, HugePageAlign::Both)
            }
        }
    }

    fn maybe_populate(&self, allocation: NonNull<[u8]>) -> NonNull<[u8]> {
        if self.options.populate {
            // SAFETY: the allocation is fresh, so nothing else accesses it yet.
            unsafe { populate(allocation.cast(), allocation.len()) };
        }
        allocation
    }

    fn allocate_transparent(
        layout: Layout,
        aligned_edges: HugePageAlign,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // `AllocError` carries no cause, so log the OS error before dropping it.
        let ptr = map(layout, aligned_edges).map_err(|error| {
            log::warn!("mmap of {} bytes failed: {error}", layout.size());
            AllocError
        })?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    fn allocate_with(
        &self,
        layout: Layout,
        global: impl FnOnce(Layout) -> Result<NonNull<[u8]>, AllocError>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = match self.huge_page_mode(layout) {
            Some(HugePageMode::Transparent(aligned_edges, allocated)) => {
                Self::allocate_transparent(allocated, aligned_edges)
            }
            Some(HugePageMode::AttemptReserved(allocated)) => {
                Self::allocate_reserved_or_transparent(allocated)
            }
            None => global(layout),
        }?;
        Ok(self.maybe_populate(allocation))
    }
}

fn round_up_to_huge_pages(layout: Layout) -> Option<Layout> {
    let size = layout.size().checked_next_multiple_of(HUGE_PAGE_SIZE)?;
    Layout::from_size_align(size, layout.align()).ok()
}

enum HugePageMode {
    Transparent(HugePageAlign, Layout),
    AttemptReserved(Layout),
}

unsafe impl Allocator for PageAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate_with(layout, |layout| Global.allocate(layout))
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // New mappings are zeroed, so skip the default memset, which would fault in every page.
        self.allocate_with(layout, |layout| Global.allocate_zeroed(layout))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: `allocate` chose the same path for this layout.
        unsafe {
            match self.huge_page_mode(layout) {
                Some(
                    HugePageMode::Transparent(_, allocated)
                    | HugePageMode::AttemptReserved(allocated),
                ) => unmap(ptr, allocated),
                None => Global.deallocate(ptr, layout),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::AlignedBuffer, allocator_api2::vec::Vec};

    #[test]
    fn test_round_trip_through_growth() {
        for alloc in [
            PageAllocator::new(AllocOptions::new().huge_pages(HugePages::Off)),
            PageAllocator::new(
                AllocOptions::new().huge_pages(HugePages::Transparent(HugePageAlign::Start)),
            ),
            PageAllocator::new(
                AllocOptions::new().huge_pages(HugePages::Transparent(HugePageAlign::End)),
            ),
            PageAllocator::new(
                AllocOptions::new().huge_pages(HugePages::Transparent(HugePageAlign::Both)),
            ),
            PageAllocator::new(AllocOptions::new().huge_pages(HugePages::AttemptReserved)),
            PageAllocator::new(AllocOptions::new().populate(true)),
            PageAllocator::new(
                AllocOptions::new()
                    .huge_pages(HugePages::Transparent(HugePageAlign::Start))
                    .populate(true),
            ),
        ] {
            let mut vec = Vec::with_capacity_in(HUGE_PAGE_SIZE / 16, alloc);
            vec.extend(0..(HUGE_PAGE_SIZE / 2) as u64);
            assert!(vec.iter().enumerate().all(|(i, &v)| v == i as u64));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_huge_page_placement() {
        let layout = Layout::from_size_align(HUGE_PAGE_SIZE + 1, 1).unwrap();
        for (huge_pages, len) in [
            (HugePages::Transparent(HugePageAlign::Start), layout.size()),
            (HugePages::Transparent(HugePageAlign::End), layout.size()),
            (
                HugePages::Transparent(HugePageAlign::Both),
                2 * HUGE_PAGE_SIZE,
            ),
            (HugePages::AttemptReserved, 2 * HUGE_PAGE_SIZE),
        ] {
            let alloc = PageAllocator::new(AllocOptions::new().huge_pages(huge_pages));
            let ptr = alloc.allocate_zeroed(layout).unwrap();
            assert_eq!(ptr.len(), len);
            let start = ptr.cast::<u8>().addr().get();
            let boundary = match huge_pages {
                HugePages::Transparent(HugePageAlign::End) => {
                    (start + len).next_multiple_of(crate::page_size())
                }
                _ => start,
            };
            assert!(boundary.is_multiple_of(HUGE_PAGE_SIZE));
            // SAFETY: `alloc` returned `ptr` for `layout`, and nothing else references it.
            unsafe {
                let bytes = ptr.as_ptr().as_mut().unwrap();
                assert!(bytes.iter().all(|&b| b == 0));
                bytes.fill(1);
                alloc.deallocate(ptr.cast(), layout);
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_populate_makes_pages_resident() {
        let page = crate::page_size();
        for (huge_pages, size) in [
            (HugePages::Off, 3 * page + 100),
            (
                HugePages::Transparent(HugePageAlign::End),
                2 * HUGE_PAGE_SIZE + page,
            ),
        ] {
            let options = AllocOptions::new().huge_pages(huge_pages).populate(true);
            let layout = Layout::from_size_align(size, 1).unwrap();
            let buf = AlignedBuffer::new_with_options(layout, options).unwrap();
            let start = buf.as_ptr().addr();
            let page_start = start & !(page - 1);
            let len = (start + size).next_multiple_of(page) - page_start;
            let mut residency = vec![0u8; len / page];
            // SAFETY: the pages hold the buffer, and `residency` has one byte per page.
            let result =
                unsafe { libc::mincore(page_start as *mut _, len, residency.as_mut_ptr().cast()) };
            assert_eq!(result, 0);
            assert!(residency.iter().all(|&r| r & 1 == 1));
            assert!(buf.iter().all(|&b| b == 0));
        }
    }
}
