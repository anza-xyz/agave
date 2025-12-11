//! Utility modules for syscall harness.

pub mod err_map;
pub mod mem_regions;

pub const STACK_SIZE: usize = 64 * STACK_GAP_SIZE as usize;
pub const STACK_GAP_SIZE: u64 = 4_096;
pub const HEAP_MAX: usize = 256 * 1024;
