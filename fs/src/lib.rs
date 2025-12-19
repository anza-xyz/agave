#![cfg(feature = "agave-unstable-api")]

pub mod buffered_reader;
pub mod buffered_writer;
pub mod dirs;
mod file_info;
pub mod file_io;
pub mod io_setup;
mod io_uring;

pub use file_info::FileInfo;

/// Alias for file offsets and sizes - since files can exceed 4GB, use 64-bits
pub type FileSize = u64;

/// Single IO performed on a filesystem can never exceed 32-bits,
/// this also constrains possible buffer sizes that are used for IO operations.
pub type IoSize = u32;
