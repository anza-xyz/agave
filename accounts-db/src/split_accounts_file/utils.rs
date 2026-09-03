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
pub fn file_offset_from_logical(logical_offset: LogicalOffset) -> FileOffset {
    FileOffset((logical_offset.0 as FileSize) << meta::META_ENTRY_OFFSET_ALIGNMENT_LOG2)
}

/// Returns logical offset from `file_offset`.
///
/// Panics if `file_offset` is not properly aligned.
pub fn logical_offset_from_file(
    file_offset: FileOffset,
) -> Result<LogicalOffset, SplitAccountsFileError> {
    // it is a programmer bug if `file_offset` is not properly aligned
    assert!(
        file_offset
            .0
            .is_multiple_of(meta::META_ENTRY_OFFSET_ALIGNMENT as FileSize)
    );
    let logical_offset = file_offset.0 >> meta::META_ENTRY_OFFSET_ALIGNMENT_LOG2 as FileSize;
    let logical_offset = u32::try_from(logical_offset)
        .map_err(|_err| SplitAccountsFileError::InvalidFileOffset(file_offset))?;
    Ok(LogicalOffset(logical_offset))
}
