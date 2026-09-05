use {
    super::FileOffset,
    semver::Version,
    solana_pubkey::Pubkey,
    std::{io, path::PathBuf},
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("failed to create new file '{1}': {0}")]
    CreateNewFile(#[source] io::Error, PathBuf),

    #[error("failed to open file '{1}': {0}")]
    OpenFile(#[source] io::Error, PathBuf),

    #[error("failed to flush file: {0}")]
    FlushFile(#[source] io::Error),

    #[error("not writable")]
    NotWritable,

    #[error("invalid data len: {0}")]
    DataLen(#[from] DataLenError),

    #[error("failed to write meta header: {0}")]
    WriteMetaHeader(#[from] WriteMetaHeaderError),

    #[error("failed to write meta entry: {0}")]
    WriteMetaEntry(#[from] WriteMetaEntryError),

    #[error("failed to write data header: {0}")]
    WriteDataHeader(#[from] WriteDataHeaderError),

    #[error("failed to write data entry: {0}")]
    WriteDataEntry(#[from] WriteDataEntryError),

    #[error("failed to read meta header: {0}")]
    ReadMetaHeader(#[from] ReadMetaHeaderError),

    #[error("failed to read meta entry: {0}")]
    ReadMetaEntry(#[from] ReadMetaEntryError),

    #[error("failed to read data header: {0}")]
    ReadDataHeader(#[from] ReadDataHeaderError),

    #[error("failed to read data entry: {0}")]
    ReadDataEntry(#[from] ReadDataEntryError),

    #[error("file offset is invalid: {}", .0.0)]
    InvalidFileOffset(FileOffset),

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum WriteMetaHeaderError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum WriteMetaEntryError {
    #[error("offset overruns file: {}", .0.0)]
    OffsetOverrun(FileOffset),

    #[error("did not write expected number of bytes: expected {expected}, actual: {actual}")]
    ShortWrite { expected: usize, actual: usize },

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum WriteDataHeaderError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum WriteDataEntryError {
    #[error("invalid data len: {0}")]
    DataLen(#[from] DataLenError),

    #[error("offset overruns file: {}", .0.0)]
    OffsetOverrun(FileOffset),

    #[error("did not write expected number of bytes: expected {expected}, actual: {actual}")]
    ShortWrite { expected: usize, actual: usize },

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum ReadMetaHeaderError {
    #[error("did not read expected number of bytes: expected {expected}, actual: {actual}")]
    ShortRead { expected: usize, actual: usize },

    #[error("invalid magic")]
    InvalidMagic,

    #[error("invalid format version: {0}")]
    InvalidFormatVersion(Version),

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum ReadMetaEntryError {
    #[error("offset is unaligned: {}", .0.0)]
    OffsetUnaligned(FileOffset),

    #[error("offset overruns file: {}", .0.0)]
    OffsetOverrun(FileOffset),

    #[error("did not read expected number of bytes: expected {expected}, actual: {actual}")]
    ShortRead { expected: usize, actual: usize },

    #[error("value for is_executable is invalid: {0}")]
    InvalidIsExecutable(u8),

    #[error("invalid data_len: {0}")]
    InvalidDataLen(#[from] DataLenError),

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum ReadDataHeaderError {
    #[error("did not read expected number of bytes: expected {expected}, actual: {actual}")]
    ShortRead { expected: usize, actual: usize },

    #[error("invalid magic")]
    InvalidMagic,

    #[error("invalid format version: {0}")]
    InvalidFormatVersion(Version),

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum ReadDataEntryError {
    #[error("offset is unaligned: {}", .0.0)]
    OffsetUnaligned(FileOffset),

    #[error("offset overruns file: {}", .0.0)]
    OffsetOverrun(FileOffset),

    #[error("address does not match: expected {expected}, actual: {actual}")]
    AddressMismatch { expected: Pubkey, actual: Pubkey },

    #[error("data len does not match: expected {expected}, actual: {actual}")]
    DataLenMismatch { expected: u32, actual: u32 },

    #[error("did not read expected number of bytes: expected {expected}, actual: {actual}")]
    ShortRead { expected: usize, actual: usize },

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Error, Debug)]
pub enum DataLenError {
    #[error("data len too large: {0}, max: {1}")]
    TooLarge(usize, u32),
}
