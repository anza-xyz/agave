#![allow(clippy::arithmetic_side_effects)]

use {
    libc::{_SC_PAGESIZE, munmap, sysconf},
    std::{
        ffi::c_void,
        io,
        marker::PhantomData,
        ops::{Deref, DerefMut},
        ptr, slice,
    },
};

#[derive(Copy, Clone, Debug)]
pub struct FrameOffset(pub(crate) usize);

pub trait Frame {
    fn offset(&self) -> FrameOffset;
    fn len(&self) -> usize;
    fn set_len(&mut self, len: usize);
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub trait Umem {
    type Frame: Frame;
    fn as_ptr(&self) -> *const u8;
    fn as_mut_ptr(&mut self) -> *mut u8;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn reserve(&mut self) -> Option<Self::Frame>;
    fn release(&mut self, frame: FrameOffset);
    fn frame_size(&self) -> usize;
    fn capacity(&self) -> usize;
    fn available(&self) -> usize;
    fn map_frame(&self, frame: &Self::Frame) -> &[u8] {
        unsafe { slice::from_raw_parts(self.as_ptr().add(frame.offset().0), frame.len()) }
    }
    fn map_frame_mut(&mut self, frame: &Self::Frame) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.as_mut_ptr().add(frame.offset().0), frame.len()) }
    }
}

pub struct SliceUmemFrame<'a> {
    offset: usize,
    len: usize,
    _buf: PhantomData<&'a mut [u8]>,
}

impl Frame for SliceUmemFrame<'_> {
    fn offset(&self) -> FrameOffset {
        FrameOffset(self.offset)
    }

    fn len(&self) -> usize {
        self.len
    }

    fn set_len(&mut self, len: usize) {
        self.len = len;
    }
}

pub struct SliceUmem<'a> {
    buffer: &'a mut [u8],
    frame_size: u32,
    available_frames: Vec<u64>,
    capacity: usize,
}

impl<'a> SliceUmem<'a> {
    pub fn new(buffer: &'a mut [u8], frame_size: u32) -> Result<Self, io::Error> {
        debug_assert!(frame_size.is_power_of_two());
        let capacity = buffer.len() / frame_size as usize;
        Ok(Self {
            available_frames: Vec::from_iter(0..capacity as u64),
            capacity,
            frame_size,
            buffer,
        })
    }
}

impl<'a> Umem for SliceUmem<'a> {
    type Frame = SliceUmemFrame<'a>;

    fn as_ptr(&self) -> *const u8 {
        self.buffer.as_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.buffer.as_mut_ptr()
    }

    fn len(&self) -> usize {
        self.buffer.len()
    }

    fn frame_size(&self) -> usize {
        self.frame_size as usize
    }

    fn reserve(&mut self) -> Option<SliceUmemFrame<'a>> {
        let index = self.available_frames.pop()?;

        Some(SliceUmemFrame {
            offset: index as usize * self.frame_size as usize,
            len: 0,
            _buf: PhantomData,
        })
    }

    fn release(&mut self, frame: FrameOffset) {
        let index = frame.0 / self.frame_size as usize;
        self.available_frames.push(index as u64);
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn available(&self) -> usize {
        self.available_frames.len()
    }
}

pub struct OwnedUmemFrame {
    offset: usize,
    len: usize,
}

impl Frame for OwnedUmemFrame {
    fn offset(&self) -> FrameOffset {
        FrameOffset(self.offset)
    }

    fn len(&self) -> usize {
        self.len
    }

    fn set_len(&mut self, len: usize) {
        self.len = len;
    }
}

pub struct OwnedUmem<T: DerefMut<Target = [u8]>> {
    owned: T,
    frame_size: u32,
    available_frames: Vec<u64>,
    capacity: usize,
}

impl<T: DerefMut<Target = [u8]>> OwnedUmem<T> {
    pub fn new(owned: T, frame_size: u32) -> Result<Self, io::Error> {
        debug_assert!(frame_size.is_power_of_two());
        let capacity = owned.len() / frame_size as usize;
        Ok(Self {
            owned,
            frame_size,
            available_frames: Vec::from_iter(0..capacity as u64),
            capacity,
        })
    }
}

impl<T: DerefMut<Target = [u8]>> Umem for OwnedUmem<T> {
    type Frame = OwnedUmemFrame;

    fn as_ptr(&self) -> *const u8 {
        self.owned.as_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.owned.as_mut_ptr()
    }

    fn len(&self) -> usize {
        self.owned.len()
    }

    fn frame_size(&self) -> usize {
        self.frame_size as usize
    }

    fn reserve(&mut self) -> Option<OwnedUmemFrame> {
        let index = self.available_frames.pop()?;

        Some(OwnedUmemFrame {
            offset: index as usize * self.frame_size as usize,
            len: 0,
        })
    }

    fn release(&mut self, frame: FrameOffset) {
        let index = frame.0 / self.frame_size as usize;
        self.available_frames.push(index as u64);
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn available(&self) -> usize {
        self.available_frames.len()
    }
}

#[derive(Debug)]
pub struct AllocError;

pub struct PageAlignedMemory {
    ptr: *mut u8,
    len: usize,
}

/// Safety: a `PageAlignedMemory` instance MUST only be resident in one thread at a time
unsafe impl Send for PageAlignedMemory {}

impl PageAlignedMemory {
    pub fn alloc(frame_size: usize, frame_count: usize) -> Result<Self, AllocError> {
        Self::alloc_with_page_size(
            frame_size,
            frame_count,
            // Safety: just a libc wrapper
            unsafe { sysconf(_SC_PAGESIZE) as usize },
            false,
        )
    }

    pub fn alloc_with_page_size(
        frame_size: usize,
        frame_count: usize,
        page_size: usize,
        huge: bool,
    ) -> Result<Self, AllocError> {
        debug_assert!(frame_size.is_power_of_two());
        debug_assert!(frame_count.is_power_of_two());
        debug_assert!(page_size.is_power_of_two());
        let memory_size = frame_count * frame_size;
        let aligned_size = (memory_size + page_size - 1) & !(page_size - 1);

        // Safety:
        // doing an ANONYMOUS alloc. addr=NULL is ok, fd is not used.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                aligned_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | if huge { libc::MAP_HUGETLB } else { 0 },
                -1,
                0,
            )
        };

        if std::ptr::eq(ptr, libc::MAP_FAILED) {
            return Err(AllocError);
        }

        // Safety: ptr is valid for aligned_size bytes
        unsafe {
            ptr::write_bytes(ptr as *mut u8, 0, aligned_size);
        }

        Ok(Self {
            ptr: ptr as *mut u8,
            len: aligned_size,
        })
    }
}

impl Drop for PageAlignedMemory {
    fn drop(&mut self) {
        // Safety:
        // ptr is a valid pointer returned by mmap
        unsafe {
            munmap(self.ptr as *mut c_void, self.len);
        }
    }
}

impl Deref for PageAlignedMemory {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for PageAlignedMemory {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}
