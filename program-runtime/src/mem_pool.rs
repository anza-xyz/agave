use {
    crate::execution_budget::{
        MAX_CALL_DEPTH, MAX_HEAP_FRAME_BYTES, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268,
        MIN_HEAP_FRAME_BYTES,
    },
    solana_sbpf::{aligned_memory::AlignedMemory, ebpf::HOST_ALIGN, vm::CallFrame},
    std::{
        array,
        ops::{Deref, DerefMut},
    },
};

trait Reset {
    fn reset(&mut self, len: usize);
}

struct Pool<T: Reset, const SIZE: usize> {
    items: [Option<T>; SIZE],
    next_empty: usize,
}

impl<T: Reset, const SIZE: usize> Pool<T, SIZE> {
    fn new(items: [T; SIZE]) -> Self {
        Self {
            items: items.map(|i| Some(i)),
            next_empty: SIZE,
        }
    }

    fn len(&self) -> usize {
        SIZE
    }

    fn get(&mut self) -> Option<T> {
        if self.next_empty == 0 {
            return None;
        }
        self.next_empty = self.next_empty.saturating_sub(1);
        self.items
            .get_mut(self.next_empty)
            .and_then(|item| item.take())
    }

    fn put(&mut self, mut value: T, len: usize) -> bool {
        self.items
            .get_mut(self.next_empty)
            .map(|item| {
                value.reset(len);
                item.replace(value);
                self.next_empty = self.next_empty.saturating_add(1);
                true
            })
            .unwrap_or(false)
    }
}

impl Reset for AlignedMemory<{ HOST_ALIGN }> {
    fn reset(&mut self, len: usize) {
        let slice = self.as_slice_mut();
        let len = len.min(slice.len());
        if let Some(head) = slice.get_mut(..len) {
            head.fill(0);
        }
    }
}

pub struct CallFrameBuffer(Box<[CallFrame; MAX_CALL_DEPTH]>);

impl Default for CallFrameBuffer {
    fn default() -> Self {
        let mut mem = Box::<[CallFrame; MAX_CALL_DEPTH]>::new_uninit();
        let ptr = mem.as_mut_ptr().cast::<CallFrame>();
        for i in 0..MAX_CALL_DEPTH {
            unsafe { ptr.add(i).write(CallFrame::default()) }
        }
        Self(unsafe { mem.assume_init() })
    }
}

impl Reset for CallFrameBuffer {
    fn reset(&mut self, _len: usize) {
        self.fill(CallFrame::default())
    }
}

impl Deref for CallFrameBuffer {
    type Target = [CallFrame];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl DerefMut for CallFrameBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut_slice()
    }
}

pub struct VmMemoryPool {
    stack: Pool<AlignedMemory<{ HOST_ALIGN }>, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268>,
    heap: Pool<AlignedMemory<{ HOST_ALIGN }>, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268>,
    call_frame: Pool<CallFrameBuffer, MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268>,
}

impl VmMemoryPool {
    pub fn new() -> Self {
        Self {
            stack: Pool::new(array::from_fn(|_| {
                #[allow(clippy::arithmetic_side_effects)]
                AlignedMemory::zero_filled(solana_sbpf::vm::get_stack_frame_size() * MAX_CALL_DEPTH)
            })),
            heap: Pool::new(array::from_fn(|_| {
                AlignedMemory::zero_filled(MAX_HEAP_FRAME_BYTES as usize)
            })),
            call_frame: Pool::new(array::from_fn(|_| CallFrameBuffer::default())),
        }
    }

    pub fn stack_len(&self) -> usize {
        self.stack.len()
    }

    pub fn heap_len(&self) -> usize {
        self.heap.len()
    }

    #[allow(clippy::arithmetic_side_effects)]
    pub fn get_stack(&mut self, size: usize) -> AlignedMemory<{ HOST_ALIGN }> {
        debug_assert!(size == solana_sbpf::vm::get_stack_frame_size() * MAX_CALL_DEPTH);
        self.stack
            .get()
            .unwrap_or_else(|| AlignedMemory::zero_filled(size))
    }

    pub fn put_stack(&mut self, stack: AlignedMemory<{ HOST_ALIGN }>) -> bool {
        let len = stack.len();
        self.stack.put(stack, len)
    }

    pub fn get_heap(&mut self, heap_size: u32) -> AlignedMemory<{ HOST_ALIGN }> {
        debug_assert!((MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&heap_size));
        self.heap
            .get()
            .unwrap_or_else(|| AlignedMemory::zero_filled(MAX_HEAP_FRAME_BYTES as usize))
    }

    pub fn put_heap(&mut self, heap: AlignedMemory<{ HOST_ALIGN }>, mapped_len: usize) -> bool {
        let heap_size = heap.len();
        debug_assert!(
            heap_size >= MIN_HEAP_FRAME_BYTES as usize
                && heap_size <= MAX_HEAP_FRAME_BYTES as usize
        );
        debug_assert!(mapped_len <= heap_size);
        self.heap.put(heap, mapped_len.min(heap_size))
    }

    pub fn get_call_frames(&mut self) -> CallFrameBuffer {
        self.call_frame.get().unwrap_or_default()
    }

    pub fn put_call_frames(&mut self, call_frame: CallFrameBuffer) -> bool {
        self.call_frame.put(call_frame, 0)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct Item(u8, u8);
    impl Reset for Item {
        fn reset(&mut self, _len: usize) {
            self.1 = 0;
        }
    }

    #[test]
    fn test_heap_shrink_then_grow_stays_zeroed() {
        let mut pool = VmMemoryPool::new();
        let big = MAX_HEAP_FRAME_BYTES;
        let small = MIN_HEAP_FRAME_BYTES;

        let mut heap = pool.get_heap(big);
        heap.as_slice_mut().fill(0xaa);
        assert!(pool.put_heap(heap, big as usize));

        let mut heap = pool.get_heap(small);
        heap.as_slice_mut()
            .get_mut(..small as usize)
            .unwrap()
            .fill(0xbb);
        assert!(pool.put_heap(heap, small as usize));

        let heap = pool.get_heap(big);
        assert!(heap.as_slice().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn test_pool() {
        let mut pool = Pool::<Item, 2>::new([Item(0, 1), Item(1, 1)]);
        assert_eq!(pool.get(), Some(Item(1, 1)));
        assert_eq!(pool.get(), Some(Item(0, 1)));
        assert_eq!(pool.get(), None);
        pool.put(Item(1, 1), 0);
        assert_eq!(pool.get(), Some(Item(1, 0)));
        pool.put(Item(2, 2), 0);
        pool.put(Item(3, 3), 0);
        assert!(!pool.put(Item(4, 4), 0));
        assert_eq!(pool.get(), Some(Item(3, 0)));
        assert_eq!(pool.get(), Some(Item(2, 0)));
        assert_eq!(pool.get(), None);
    }
}
