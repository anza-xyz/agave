use {
    super::{
        SplitAccountsFileError,
        common::{FileOffset, LogicalOffset},
        meta,
    },
    agave_fs::FileSize,
    std::{convert::TryFrom, fs::File, path::Path},
};

/// Creates a new file at `path`.
pub fn create_new_file(path: impl AsRef<Path>) -> Result<File, SplitAccountsFileError> {
    File::create_new(&path)
        .map_err(|err| SplitAccountsFileError::CreateNewFile(err, path.as_ref().to_path_buf()))
}

/// Opens an existing file at `path`.
pub fn open_file(path: impl AsRef<Path>) -> Result<File, SplitAccountsFileError> {
    File::open(&path)
        .map_err(|err| SplitAccountsFileError::OpenFile(err, path.as_ref().to_path_buf()))
}

/// Returns file offset from `logical_offset`.
pub fn file_offset_from_logical(
    logical_offset: LogicalOffset,
) -> Result<FileOffset, SplitAccountsFileError> {
    let offset = (logical_offset.0 as FileSize)
        .checked_shl(meta::META_ENTRY_OFFSET_ALIGNMENT_LOG2)
        .ok_or(SplitAccountsFileError::InvalidLogicalOffset(logical_offset))?;
    Ok(FileOffset(offset))
}

/// Returns logical offset from `file_offset`.
pub fn logical_offset_from_file(
    file_offset: FileOffset,
) -> Result<LogicalOffset, SplitAccountsFileError> {
    const {
        assert!(meta::META_ENTRY_OFFSET_ALIGNMENT_LOG2 < FileSize::BITS);
    };
    // SAFETY: The shift-right-amount is less than the number of bits in the file offset.
    let offset = unsafe {
        file_offset
            .0
            .unchecked_shr(meta::META_ENTRY_OFFSET_ALIGNMENT_LOG2)
    };
    let offset = u32::try_from(offset)
        .map_err(|_err| SplitAccountsFileError::InvalidFileOffset(file_offset))?;
    Ok(LogicalOffset(offset))
}
