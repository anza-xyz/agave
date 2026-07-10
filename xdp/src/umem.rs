#![allow(clippy::arithmetic_side_effects)]

use {
    crossbeam_queue::ArrayQueue,
    libc::{_SC_PAGESIZE, munmap, sysconf},
    std::{
        ffi::c_void,
        io,
        marker::PhantomData,
        ops::{Deref, DerefMut},
        ptr, slice,
        sync::Arc,
    },
};

#[derive(Debug)]
pub struct FrameOffset(pub(crate) usize);

pub struct CompletedFrameOffset(pub(crate) FrameOffset);

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
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn reserve(&self) -> Option<Self::Frame>;
    fn release(&self, frame: Self::Frame);
    fn release_completed(&self, frame: CompletedFrameOffset);
    fn frame_size(&self) -> usize;
    fn capacity(&self) -> usize;
    fn available(&self) -> usize;
    fn map_frame_mut(&self, frame: Self::Frame) -> MappedFrameMut<'_, Self::Frame>;
}

impl<U: Umem> Umem for Arc<U> {
    type Frame = U::Frame;

    fn as_ptr(&self) -> *const u8 {
        self.as_ref().as_ptr()
    }

    fn len(&self) -> usize {
        self.as_ref().len()
    }

    fn reserve(&self) -> Option<Self::Frame> {
        self.as_ref().reserve()
    }

    fn release(&self, frame: Self::Frame) {
        self.as_ref().release(frame);
    }

    fn release_completed(&self, frame: CompletedFrameOffset) {
        self.as_ref().release_completed(frame);
    }

    fn frame_size(&self) -> usize {
        self.as_ref().frame_size()
    }

    fn capacity(&self) -> usize {
        self.as_ref().capacity()
    }

    fn available(&self) -> usize {
        self.as_ref().available()
    }

    fn map_frame_mut(&self, frame: Self::Frame) -> MappedFrameMut<'_, Self::Frame> {
        self.as_ref().map_frame_mut(frame)
    }
}

pub struct MappedFrameMut<'a, F> {
    frame: F,
    bytes: &'a mut [u8],
}

impl<F> MappedFrameMut<'_, F> {
    pub fn into_frame(self) -> F {
        let Self { frame, bytes: _ } = self;
        frame
    }
}

impl<F> Deref for MappedFrameMut<'_, F> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.bytes
    }
}

impl<F> DerefMut for MappedFrameMut<'_, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.bytes
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
    buffer: *mut u8,
    buffer_len: usize,
    frame_size: u32,
    available_frames: ArrayQueue<FrameOffset>,
    _buffer: PhantomData<&'a mut [u8]>,
}

// Safety: `SliceUmem` retains the exclusive borrow of `buffer`, and frame tokens ensure that
// concurrent mappings refer to non aliasing regions.
unsafe impl Send for SliceUmem<'_> {}
unsafe impl Sync for SliceUmem<'_> {}

impl<'a> SliceUmem<'a> {
    pub fn new(buffer: &'a mut [u8], frame_size: u32) -> Result<Self, io::Error> {
        debug_assert!(frame_size.is_power_of_two());
        let capacity = buffer.len() / frame_size as usize;
        Ok(Self {
            buffer: buffer.as_mut_ptr(),
            buffer_len: buffer.len(),
            available_frames: available_frames(capacity, frame_size),
            frame_size,
            _buffer: PhantomData,
        })
    }
}

impl<'a> Umem for SliceUmem<'a> {
    type Frame = SliceUmemFrame<'a>;

    fn as_ptr(&self) -> *const u8 {
        self.buffer.cast_const()
    }

    fn len(&self) -> usize {
        self.buffer_len
    }

    fn frame_size(&self) -> usize {
        self.frame_size as usize
    }

    fn reserve(&self) -> Option<SliceUmemFrame<'a>> {
        let FrameOffset(offset) = self.available_frames.pop()?;

        Some(SliceUmemFrame {
            offset,
            len: 0,
            _buf: PhantomData,
        })
    }

    fn release(&self, frame: Self::Frame) {
        push_available_frame(&self.available_frames, frame.offset());
    }

    fn release_completed(&self, frame: CompletedFrameOffset) {
        push_available_frame(&self.available_frames, frame.0);
    }

    fn capacity(&self) -> usize {
        self.available_frames.capacity()
    }

    fn available(&self) -> usize {
        self.available_frames.len()
    }

    fn map_frame_mut(&self, frame: Self::Frame) -> MappedFrameMut<'_, Self::Frame> {
        // Safety: `frame` is the ownership token for a reserved UMEM slot. This method consumes
        // it, so safe callers cannot map or release the same slot again while `bytes` is live.
        let bytes =
            unsafe { slice::from_raw_parts_mut(self.buffer.add(frame.offset().0), frame.len()) };
        MappedFrameMut { frame, bytes }
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

pub struct OwnedUmem {
    memory: PageAlignedMemory,
    frame_size: u32,
    available_frames: ArrayQueue<FrameOffset>,
}

impl OwnedUmem {
    pub fn new(memory: PageAlignedMemory, frame_size: u32) -> Result<Self, io::Error> {
        debug_assert!(frame_size.is_power_of_two());
        let capacity = memory.len / frame_size as usize;
        Ok(Self {
            memory,
            frame_size,
            available_frames: available_frames(capacity, frame_size),
        })
    }
}

impl Umem for OwnedUmem {
    type Frame = OwnedUmemFrame;

    fn as_ptr(&self) -> *const u8 {
        self.memory.ptr.cast_const()
    }

    fn len(&self) -> usize {
        self.memory.len
    }

    fn frame_size(&self) -> usize {
        self.frame_size as usize
    }

    fn reserve(&self) -> Option<OwnedUmemFrame> {
        let FrameOffset(offset) = self.available_frames.pop()?;

        Some(OwnedUmemFrame { offset, len: 0 })
    }

    fn release(&self, frame: Self::Frame) {
        push_available_frame(&self.available_frames, frame.offset());
    }

    fn release_completed(&self, frame: CompletedFrameOffset) {
        push_available_frame(&self.available_frames, frame.0);
    }

    fn capacity(&self) -> usize {
        self.available_frames.capacity()
    }

    fn available(&self) -> usize {
        self.available_frames.len()
    }

    fn map_frame_mut(&self, frame: Self::Frame) -> MappedFrameMut<'_, Self::Frame> {
        // Safety: `frame` is the ownership token for a reserved UMEM slot. This method consumes
        // it, so safe callers cannot map or release the same slot again while `bytes` is live.
        let bytes = unsafe {
            slice::from_raw_parts_mut(self.memory.ptr.add(frame.offset().0), frame.len())
        };
        MappedFrameMut { frame, bytes }
    }
}

fn available_frames(capacity: usize, frame_size: u32) -> ArrayQueue<FrameOffset> {
    let available_frames = ArrayQueue::new(capacity);
    for index in 0..capacity {
        push_available_frame(&available_frames, FrameOffset(index * frame_size as usize));
    }
    available_frames
}

fn push_available_frame(available_frames: &ArrayQueue<FrameOffset>, offset: FrameOffset) {
    available_frames
        .push(offset)
        .expect("available UMEM frame queue unexpectedly full");
}

#[derive(Debug)]
pub struct AllocError;

pub struct PageAlignedMemory {
    ptr: *mut u8,
    len: usize,
}

// Safety: `PageAlignedMemory` owns its mapping and may be moved between threads.
unsafe impl Send for PageAlignedMemory {}
// Safety: `PageAlignedMemory` doesn't expose interior mutability so it's safe to share between threads.
unsafe impl Sync for PageAlignedMemory {}

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

#[cfg(test)]
mod tests {
    use {
        crate::umem::{CompletedFrameOffset, Frame, SliceUmem, Umem},
        std::slice,
    };

    #[test]
    fn test_map_frame() {
        let mut buffer = [0; 16];
        let umem = SliceUmem::new(&mut buffer, 8).unwrap();
        let mut frame = umem.reserve().unwrap();
        let offset = frame.offset().0;

        frame.set_len(4);
        let mut mapped = umem.map_frame_mut(frame);
        mapped.copy_from_slice(&[1, 2, 3, 4]);
        let frame = mapped.into_frame();

        // Safety: `offset` came from a reserved frame and `frame.len()` bytes were initialized
        // through the mapping above.
        let bytes = unsafe { slice::from_raw_parts(umem.as_ptr().add(offset), frame.len()) };
        assert_eq!(bytes, &[1, 2, 3, 4]);

        umem.release(frame);
        assert_eq!(umem.available(), umem.capacity());
    }

    #[test]
    fn test_completion() {
        let mut buffer = [0; 16];
        let umem = SliceUmem::new(&mut buffer, 8).unwrap();
        let offset = {
            let frame = umem.reserve().unwrap();
            frame.offset()
        };
        umem.release_completed(CompletedFrameOffset(offset));

        assert_eq!(umem.available(), umem.capacity());
    }
}
