//! Error types and helpers.
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Attempting to read {0} bytes")]
    ReadSizeLimit(usize),
    #[error("Attempting to write {0} bytes")]
    WriteSizeLimit(usize),
    #[error(
        "Encoded sequence length exceeded preallocation limit of {limit} bytes (needed {needed} \
         bytes)"
    )]
    PreallocationSizeLimit { needed: usize, limit: usize },
    #[error("Encoded sequence length would overflow {0}")]
    SizeHintOverflow(&'static str),
    #[error("Could not cast integer type to pointer sized type")]
    PointerSizedDecodeError,
    #[error("Invalid bool encoding: {0}")]
    InvalidBoolEncoding(u8),
    #[error("Invalid tag encoding: {0}")]
    InvalidTagEncoding(usize),
    #[error("Writer has trailing bytes: {0}")]
    WriterTrailingBytes(usize),
}

pub type Result<T> = core::result::Result<T, Error>;

#[cold]
pub fn read_size_limit(len: usize) -> Error {
    Error::ReadSizeLimit(len)
}

#[cold]
pub fn write_size_limit(len: usize) -> Error {
    Error::WriteSizeLimit(len)
}

#[cold]
pub fn preallocation_size_limit(needed: usize, limit: usize) -> Error {
    Error::PreallocationSizeLimit { needed, limit }
}

#[cold]
pub fn size_hint_overflow(max_length: &'static str) -> Error {
    Error::SizeHintOverflow(max_length)
}

#[cold]
pub fn pointer_sized_decode_error() -> Error {
    Error::PointerSizedDecodeError
}

#[cold]
pub fn invalid_bool_encoding(byte: u8) -> Error {
    Error::InvalidBoolEncoding(byte)
}

#[cold]
pub fn invalid_tag_encoding(tag: usize) -> Error {
    Error::InvalidTagEncoding(tag)
}

#[cold]
pub fn writer_trailing_bytes(bytes: usize) -> Error {
    Error::WriterTrailingBytes(bytes)
}
