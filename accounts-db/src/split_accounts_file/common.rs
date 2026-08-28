use {super::error::DataLenError, agave_fs::FileSize, std::convert::TryFrom};

/// A logical offset used to load an account in a SplitAccountsFile.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct LogicalOffset(pub u32);

/// The actual file offset.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct FileOffset(pub FileSize);

/// File offset of the account data that is stored in an external file.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ExternalDataOffset(pub FileOffset);

/// Information about what was written.
#[derive(Debug)]
pub struct WriteInfo {
    /// The file offset where writing began.
    pub start: FileOffset,
    /// The number of bytes written to the file.
    pub num_bytes_written: usize,
}

/// Account data length, which supports checked sizes.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct DataLen(pub u32);

impl DataLen {
    /// The maximum permitted size of account data.
    pub const MAX: u32 = 10 * 1024 * 1024;
}

impl TryFrom<usize> for DataLen {
    type Error = DataLenError;
    fn try_from(data_len: usize) -> Result<Self, Self::Error> {
        if data_len <= Self::MAX as usize {
            Ok(Self(data_len as u32))
        } else {
            Err(DataLenError::TooLarge(data_len, Self::MAX))
        }
    }
}

/// Data reference, used for writing.
#[derive(Debug, PartialEq)]
pub enum DataRefBorrowed<'data> {
    NoData,
    Inline(&'data [u8]),
    External(ExternalDataOffset),
}

#[cfg(test)]
mod tests {
    use {super::*, solana_system_interface::MAX_PERMITTED_DATA_LENGTH};

    /// Ensure DataLen::MAX stays in sync with the solana-sdk.
    #[test]
    fn test_data_len_max() {
        assert_eq!(DataLen::MAX as u64, MAX_PERMITTED_DATA_LENGTH);
    }
}
