#[cfg(target_os = "linux")]
pub(crate) use linux::{map, map_reserved, populate, unmap};

#[cfg(target_os = "linux")]
mod linux {
    use {
        crate::{HUGE_PAGE_SIZE, HugePageAlign, page_size},
        std::{
            alloc::Layout,
            io,
            ptr::{self, NonNull},
            sync::Once,
        },
    };

    /// Maps zeroed memory for `layout` and advises transparent huge pages for it.
    ///
    /// `aligned_edges` selects the edges of the mapping that lie on huge page boundaries.
    pub(crate) fn map(layout: Layout, aligned_edges: HugePageAlign) -> io::Result<NonNull<u8>> {
        let (len, align) = mapped_len_and_align(layout);
        let start = map_aligned(len, align, aligned_edges, /* flags */ 0)?;
        // SAFETY: the range is mapped and unused.
        let advised = unsafe { libc::madvise(start.as_ptr().cast(), len, libc::MADV_HUGEPAGE) };
        // The advice only affects how the kernel backs the range, so the memory stays valid in
        // base pages when it fails.
        if advised != 0 {
            static WARN_ONCE: Once = Once::new();
            let error = io::Error::last_os_error();
            let cause = if error.raw_os_error() == Some(libc::EINVAL) {
                "; the kernel likely lacks transparent huge page support \
                 (CONFIG_TRANSPARENT_HUGEPAGE)"
            } else {
                ""
            };
            WARN_ONCE.call_once(|| {
                log::warn!(
                    "large allocations use base pages, which slows access to them, because \
                     madvise(MADV_HUGEPAGE) failed: {error}{cause}"
                );
            });
        }
        Ok(start)
    }

    /// Maps zeroed memory for `layout` from the reserved huge page pool.
    ///
    /// Requires a `layout` size in whole huge pages, and fails when the pool lacks free pages.
    pub(crate) fn map_reserved(layout: Layout) -> io::Result<NonNull<u8>> {
        let (len, align) = mapped_len_and_align(layout);
        debug_assert!(len.is_multiple_of(HUGE_PAGE_SIZE));
        // An explicit size keeps 2 MiB pages on hosts with another default huge page size.
        map_aligned(
            len,
            align,
            HugePageAlign::Both,
            libc::MAP_HUGETLB | libc::MAP_HUGE_2MB,
        )
    }

    /// Pre-faults the pages under the `len` bytes at `ptr`, so first writes to them don't fault.
    ///
    /// # Safety
    ///
    /// The range must be allocated and writable, and nothing may read or write it concurrently.
    pub(crate) unsafe fn populate(ptr: NonNull<u8>, len: usize) {
        let page = page_size();
        // The advice needs a page aligned start, and the whole pages around the range are mapped.
        let page_offset = ptr.addr().get().strict_rem(page);
        let pages_start = ptr.as_ptr().wrapping_sub(page_offset);
        let pages_len = page_offset.strict_add(len).next_multiple_of(page);
        // SAFETY: the pages hold the writable range, and populating them leaves contents intact.
        let advised =
            unsafe { libc::madvise(pages_start.cast(), pages_len, libc::MADV_POPULATE_WRITE) };
        if advised != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                panic!(
                    "madvise(MADV_POPULATE_WRITE) failed: {error}; it needs Linux 5.14 or later"
                );
            }
            log::warn!("populating {len} bytes failed, so they fault in on first access: {error}");
        }
    }

    /// # Safety
    ///
    /// `ptr` must come from a [`map`] or [`map_reserved`] call with the same `layout`, and nothing
    /// may use the memory afterwards.
    pub(crate) unsafe fn unmap(ptr: NonNull<u8>, layout: Layout) {
        let (len, _) = mapped_len_and_align(layout);
        // SAFETY: the caller passes a live mapping of `len` bytes.
        unsafe { unmap_range_or_leak(ptr, len) };
    }

    fn mapped_len_and_align(layout: Layout) -> (usize, usize) {
        let align = layout.align().max(page_size());
        (layout.size().next_multiple_of(align), align)
    }

    /// Returns a pointer to a new mapping of `len` zeroed bytes with the `mmap` `flags`, placed
    /// at a multiple of `align` with its `aligned_edges` on huge page boundaries.
    ///
    /// Requires `len` in multiples of `align`, `align` in whole pages, and `len` in whole huge
    /// pages with `MAP_HUGETLB` or [`HugePageAlign::Both`].
    fn map_aligned(
        len: usize,
        align: usize,
        aligned_edges: HugePageAlign,
        flags: libc::c_int,
    ) -> io::Result<NonNull<u8>> {
        let huge_page_align = align.max(HUGE_PAGE_SIZE);
        let kernel_align = if flags & libc::MAP_HUGETLB != 0 {
            HUGE_PAGE_SIZE
        } else {
            page_size()
        };
        // Either edge of the range lies at a multiple of `kernel_align`, so it reaches the next
        // multiple of `huge_page_align` within this slack.
        let slack = huge_page_align.strict_sub(kernel_align);
        let map_len = len.checked_add(slack).ok_or(io::ErrorKind::OutOfMemory)?;
        // SAFETY: an anonymous private mapping has no preconditions.
        let map = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | flags,
                /* fd */ -1,
                /* offset */ 0,
            )
        };
        if ptr::eq(map, libc::MAP_FAILED) {
            return Err(io::Error::last_os_error());
        }
        let map = NonNull::new(map.cast::<u8>()).ok_or(io::ErrorKind::OutOfMemory)?;
        // The range shifts from the mapping's start until the edge requested by `aligned_edges`
        // reaches a multiple of `huge_page_align`.
        let edge_ptr = match aligned_edges {
            HugePageAlign::Start => map.as_ptr(),
            // The end reaches a huge page boundary too, since `len` spans whole huge pages.
            HugePageAlign::Both => map.as_ptr(),
            // Aligning the end also aligns the start to `align`, since `len` is a multiple of
            // `align`, which divides `huge_page_align`.
            HugePageAlign::End => map.as_ptr().wrapping_add(len),
        };
        let head_len = edge_ptr.align_offset(huge_page_align);
        // SAFETY: the head fits in the slack, and nothing references the fresh mapping.
        unsafe { trim(map, head_len, len, slack.strict_sub(head_len)) }
    }

    /// Unmaps the `head_len` bytes at `map` and the `tail_len` bytes after the next `kept_len`
    /// bytes, and returns a pointer to the kept bytes.
    ///
    /// Fails when an unmap fails, after unmapping whatever is still mapped.
    ///
    /// # Safety
    ///
    /// `map` must point to a mapping of `head_len + kept_len + tail_len` bytes, and nothing may
    /// reference it.
    unsafe fn trim(
        map: NonNull<u8>,
        head_len: usize,
        kept_len: usize,
        tail_len: usize,
    ) -> io::Result<NonNull<u8>> {
        let end_offset = head_len.strict_add(kept_len);
        // SAFETY: the caller passes a mapping that holds the head and kept bytes.
        let (start, end) = unsafe { (map.add(head_len), map.add(end_offset)) };
        // On failure, unmap only the part still mapped, since another thread may already reuse
        // the trimmed addresses.
        // SAFETY: the parts lie inside the mapping, and nothing references it.
        unsafe {
            if let Err(error) = try_unmap_range(map, head_len) {
                unmap_range_or_leak(map, end_offset.strict_add(tail_len));
                return Err(error);
            }
            if let Err(error) = try_unmap_range(end, tail_len) {
                unmap_range_or_leak(start, kept_len.strict_add(tail_len));
                return Err(error);
            }
        }
        Ok(start)
    }

    /// # Safety
    ///
    /// The range must lie inside a mapping, and nothing may reference it.
    unsafe fn unmap_range_or_leak(ptr: NonNull<u8>, len: usize) {
        // SAFETY: the caller's guarantees carry over.
        if let Err(error) = unsafe { try_unmap_range(ptr, len) } {
            log::warn!("munmap failed, leaking {len} bytes: {error}");
        }
    }

    /// # Safety
    ///
    /// The range must lie inside a mapping, and nothing may reference it.
    unsafe fn try_unmap_range(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: the caller passes an unreferenced part of a mapping.
        if len > 0 && unsafe { libc::munmap(ptr.as_ptr().cast(), len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_map_places_huge_page_boundary() {
            for (align, aligned_edges) in [
                (1, HugePageAlign::Start),
                (1, HugePageAlign::End),
                (2 * HUGE_PAGE_SIZE, HugePageAlign::End),
            ] {
                let layout = Layout::from_size_align(3 * HUGE_PAGE_SIZE + 4097, align).unwrap();
                let ptr = map(layout, aligned_edges).unwrap();
                let (len, _) = mapped_len_and_align(layout);
                let start = ptr.addr().get();
                let boundary = match aligned_edges {
                    HugePageAlign::End => start + len,
                    _ => start,
                };
                assert!(boundary.is_multiple_of(HUGE_PAGE_SIZE));
                assert!(start.is_multiple_of(align.max(page_size())));
                // SAFETY: the mapping holds `len` bytes and nothing else references it.
                let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), len) };
                assert!(bytes.iter().all(|&b| b == 0));
                bytes.fill(1);
                // SAFETY: `ptr` comes from `map` with `layout`, and `bytes` is no longer used.
                unsafe { unmap(ptr, layout) };
            }
        }

        #[test]
        fn test_map_reserved_aligns_whole_huge_pages() {
            for align in [1, 2 * HUGE_PAGE_SIZE] {
                let layout = Layout::from_size_align(4 * HUGE_PAGE_SIZE, align).unwrap();
                // Hosts without a reserved pool only exercise the failure.
                let Ok(ptr) = map_reserved(layout) else {
                    continue;
                };
                assert!(ptr.addr().get().is_multiple_of(align.max(HUGE_PAGE_SIZE)));
                // SAFETY: the mapping holds the layout's bytes and nothing else references it.
                let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), layout.size()) };
                assert!(bytes.iter().all(|&b| b == 0));
                bytes.fill(1);
                // SAFETY: `ptr` comes from `map_reserved` with `layout`, and `bytes` is no longer
                // used.
                unsafe { unmap(ptr, layout) };
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn map(
    _layout: std::alloc::Layout,
    _aligned_edges: crate::HugePageAlign,
) -> std::io::Result<std::ptr::NonNull<u8>> {
    unreachable!("only Linux maps allocations directly")
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn map_reserved(_layout: std::alloc::Layout) -> std::io::Result<std::ptr::NonNull<u8>> {
    unreachable!("only Linux maps allocations directly")
}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn populate(_ptr: std::ptr::NonNull<u8>, _len: usize) {}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn unmap(_ptr: std::ptr::NonNull<u8>, _layout: std::alloc::Layout) {
    unreachable!("only Linux maps allocations directly")
}
