#![cfg(feature = "agave-unstable-api")]
//! Memory allocation utilities shared by Agave crates.

mod aligned_buffer;
mod mmap;
mod page_allocator;
mod page_options;

pub use {
    aligned_buffer::AlignedBuffer,
    page_allocator::PageAllocator,
    page_options::{AllocOptions, HUGE_PAGE_SIZE, HugePageAlign, HugePages, page_size},
};
