#![cfg(feature = "agave-unstable-api")]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

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
