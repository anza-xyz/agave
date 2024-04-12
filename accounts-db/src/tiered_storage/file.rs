use {
    super::{error::TieredStorageError, footer::FOOTER_TAIL_SIZE, TieredStorageResult},
    bytemuck::{AnyBitPattern, NoUninit, Pod, Zeroable},
    solana_sdk::hash::Hash,
    std::{
        fs::{File, OpenOptions},
        io::{BufWriter, Read, Result as IoResult, Seek, SeekFrom, Write},
        mem,
        path::Path,
    },
};

/// The ending 8 bytes of a valid tiered account storage file.
pub const FILE_MAGIC_NUMBER: u64 = u64::from_le_bytes(*b"AnzaTech");

#[derive(Debug, PartialEq, Eq, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct TieredStorageMagicNumber(pub u64);

// Ensure there are no implicit padding bytes
const _: () = assert!(std::mem::size_of::<TieredStorageMagicNumber>() == 8);

impl Default for TieredStorageMagicNumber {
    fn default() -> Self {
        Self(FILE_MAGIC_NUMBER)
    }
}

#[derive(Debug)]
pub struct TieredReadableFile(pub File);

impl TieredReadableFile {
    pub fn new(file_path: impl AsRef<Path>) -> TieredStorageResult<Self> {
        let file = Self(
            OpenOptions::new()
                .read(true)
                .create(false)
                .open(&file_path)?,
        );

        file.check_magic_number()?;
        file.check_file_hash()?;

        Ok(file)
    }

    fn check_file_hash(&self) -> TieredStorageResult<()> {
        self.seek(0)?;

        let len = self.0.metadata()?.len() as usize;
        let hashed_len = len - std::mem::size_of::<Hash>() - FOOTER_TAIL_SIZE;
        let mut hasher = blake3::Hasher::new();

        const BLOCK_SIZE: usize = 4096;
        let mut buffer = [0u8; BLOCK_SIZE];
        let mut offset = 0;
        while offset < hashed_len {
            let block_size = std::cmp::min(BLOCK_SIZE, hashed_len - offset);
            self.read_bytes(&mut buffer[0..block_size])?;
            hasher.update(&buffer[0..block_size]);

            offset += block_size;
        }
        let hash = Hash::new_from_array(hasher.finalize().into());

        let mut raw_hash_from_file = [0u8; 32];
        self.read_bytes(&mut raw_hash_from_file)?;
        let hash_from_file = Hash::new_from_array(raw_hash_from_file);

        (hash == hash_from_file)
            .then(|| Ok(()))
            .unwrap_or_else(|| Err(TieredStorageError::FileHashMismatch(hash, hash_from_file)))
    }

    fn check_magic_number(&self) -> TieredStorageResult<()> {
        self.seek_from_end(-(std::mem::size_of::<TieredStorageMagicNumber>() as i64))?;
        let mut magic_number = TieredStorageMagicNumber::zeroed();
        self.read_pod(&mut magic_number)?;
        if magic_number != TieredStorageMagicNumber::default() {
            return Err(TieredStorageError::MagicNumberMismatch(
                TieredStorageMagicNumber::default().0,
                magic_number.0,
            ));
        }
        Ok(())
    }

    /// Reads a value of type `T` from the file.
    ///
    /// Type T must be plain ol' data.
    pub fn read_pod<T: NoUninit + AnyBitPattern>(&self, value: &mut T) -> IoResult<()> {
        // SAFETY: Since T is AnyBitPattern, it is safe to cast bytes to T.
        unsafe { self.read_type(value) }
    }

    /// Reads a value of type `T` from the file.
    ///
    /// Prefer `read_pod()` when possible, because `read_type()` may cause
    /// undefined behavior.
    ///
    /// # Safety
    ///
    /// Caller must ensure casting bytes to T is safe.
    /// Refer to the Safety sections in std::slice::from_raw_parts()
    /// and bytemuck's Pod and AnyBitPattern for more information.
    pub unsafe fn read_type<T>(&self, value: &mut T) -> IoResult<()> {
        let ptr = value as *mut _ as *mut u8;
        // SAFETY: The caller ensures it is safe to cast bytes to T,
        // we ensure the size is safe by querying T directly,
        // and Rust ensures ptr is aligned.
        let bytes = unsafe { std::slice::from_raw_parts_mut(ptr, mem::size_of::<T>()) };
        self.read_bytes(bytes)
    }

    pub fn seek(&self, offset: u64) -> IoResult<u64> {
        (&self.0).seek(SeekFrom::Start(offset))
    }

    pub fn seek_from_end(&self, offset: i64) -> IoResult<u64> {
        (&self.0).seek(SeekFrom::End(offset))
    }

    pub fn read_bytes(&self, buffer: &mut [u8]) -> IoResult<()> {
        (&self.0).read_exact(buffer)
    }
}

#[derive(Debug)]
pub struct TieredWritableFile {
    file: BufWriter<File>,
    hasher: blake3::Hasher,
}

impl TieredWritableFile {
    pub fn new(file_path: impl AsRef<Path>) -> IoResult<Self> {
        Ok(Self {
            file: BufWriter::new(
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(file_path)?,
            ),
            hasher: blake3::Hasher::new(),
        })
    }

    /// Writes `value` to the file.
    ///
    /// `value` must be plain ol' data.
    pub fn write_pod<T: NoUninit>(&mut self, value: &T) -> IoResult<usize> {
        // SAFETY: Since T is NoUninit, it does not contain any uninitialized bytes.
        unsafe { self.write_type(value) }
    }

    /// Writes `value` to the file.
    ///
    /// Prefer `write_pod` when possible, because `write_value` may cause
    /// undefined behavior if `value` contains uninitialized bytes.
    ///
    /// # Safety
    ///
    /// Caller must ensure casting T to bytes is safe.
    /// Refer to the Safety sections in std::slice::from_raw_parts()
    /// and bytemuck's Pod and NoUninit for more information.
    pub unsafe fn write_type<T>(&mut self, value: &T) -> IoResult<usize> {
        let ptr = value as *const _ as *const u8;
        let bytes = unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<T>()) };
        self.write_bytes(bytes)
    }

    pub fn seek(&mut self, offset: u64) -> IoResult<u64> {
        self.file.seek(SeekFrom::Start(offset))
    }

    pub fn seek_from_end(&mut self, offset: i64) -> IoResult<u64> {
        self.file.seek(SeekFrom::End(offset))
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> IoResult<usize> {
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);

        Ok(bytes.len())
    }

    pub fn hash(&self) -> Hash {
        Hash::new_from_array(self.hasher.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::tiered_storage::{
            error::TieredStorageError,
            file::{
                TieredReadableFile, TieredStorageMagicNumber, TieredWritableFile, FILE_MAGIC_NUMBER,
            },
            footer::TieredStorageFooter,
        },
        std::path::Path,
        tempfile::TempDir,
    };

    // Only write the footer and update the hash without writing the magic number
    fn write_footer_only(footer: &mut TieredStorageFooter, file: &mut TieredWritableFile) {
        // SAFETY: The footer does not contain any uninitialized bytes.
        unsafe {
            file.write_type(&footer.account_meta_format).unwrap();
            file.write_type(&footer.owners_block_format).unwrap();
            file.write_type(&footer.index_block_format).unwrap();
            file.write_type(&footer.account_block_format).unwrap();
        }

        file.write_pod(&footer.account_entry_count).unwrap();
        file.write_pod(&footer.account_meta_entry_size).unwrap();
        file.write_pod(&footer.account_block_size).unwrap();
        file.write_pod(&footer.owner_count).unwrap();
        file.write_pod(&footer.owner_entry_size).unwrap();
        file.write_pod(&footer.index_block_offset).unwrap();
        file.write_pod(&footer.owners_block_offset).unwrap();
        file.write_pod(&footer.min_account_address).unwrap();
        file.write_pod(&footer.max_account_address).unwrap();

        // everything before the FooterTail will be hashed
        footer.hash = file.hash();
        file.write_pod(&footer.hash).unwrap();

        file.write_pod(&footer.format_version).unwrap();
        file.write_pod(&footer.footer_size).unwrap();
    }

    fn generate_test_file_with_number(path: impl AsRef<Path>, number: u64) {
        let mut file = TieredWritableFile::new(path).unwrap();
        let mut footer = TieredStorageFooter::default();
        write_footer_only(&mut footer, &mut file);
        file.write_pod(&number).unwrap();
    }

    #[test]
    fn test_new() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test_new");
        generate_test_file_with_number(&path, FILE_MAGIC_NUMBER);
        assert!(TieredReadableFile::new(&path).is_ok());
    }

    #[test]
    fn test_magic_number_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test_magic_number_mismatch");
        generate_test_file_with_number(&path, !FILE_MAGIC_NUMBER);
        assert!(matches!(
            TieredReadableFile::new(&path),
            Err(TieredStorageError::MagicNumberMismatch(_, _))
        ));
    }

    #[test]
    fn test_file_hash() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test_file_hash_mismatch");
        let mut expected_footer = TieredStorageFooter {
            account_entry_count: 300,
            account_meta_entry_size: 24,
            account_block_size: 4096,
            owner_count: 250,
            owner_entry_size: 32,
            index_block_offset: 1069600,
            owners_block_offset: 1081200,
            ..TieredStorageFooter::default()
        };

        // Manually persist the footer without updating the hash
        {
            let mut file = TieredWritableFile::new(&path).unwrap();
            // SAFETY: the footer does not contain any uninitialized bytes
            unsafe {
                file.write_type(&expected_footer).unwrap();
            }
            file.write_pod(&TieredStorageMagicNumber::default())
                .unwrap();
        }

        // Reopen the same storage and expect FileHashMismatch
        {
            let result = TieredReadableFile::new(&path);
            assert!(matches!(
                result,
                Err(TieredStorageError::FileHashMismatch(_, _))
            ));
        }

        // Rewrite the same footer into a different file, but this time using
        // the standard way to persist the footer that will also update the
        // file hash.
        let path = temp_dir.path().join("test_file_hash");
        {
            let mut file = TieredWritableFile::new(&path).unwrap();
            expected_footer.write_footer_block(&mut file).unwrap()
        }

        // Reopen the same storage and expect the footer matches
        {
            let file = TieredReadableFile::new(&path).unwrap();
            let footer = TieredStorageFooter::new_from_footer_block(&file).unwrap();
            assert_eq!(expected_footer, footer);
        }
    }
}
