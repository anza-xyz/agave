use {
    crate::{
        account_info::Offset, account_storage_entry::AccountStorageEntry,
        accounts_file::OpenFileForArchive,
    },
    agave_fs::{
        buffered_reader::{self, FileBufRead},
        io_setup::IoSetupState,
    },
    solana_clock::Slot,
    std::io::{self, Read},
};

// Read-ahead buffer capacity, sized as a multiple of the default io-uring
// reader's read size (1 MiB) and large enough that almost any account storage
// file fits entirely within the buffer.
pub const ACCOUNT_STORAGE_MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;

#[cfg(not(target_os = "linux"))]
const READER_STACK_BUFFER_SIZE: usize = 64 * 1024;

/// Concrete reader type returned by [`storage_file_buf_reader`].
///
/// The concrete type is exposed (rather than `impl FileBufRead<'a>`) so callers
/// can use inherent methods like `rebind`.
#[cfg(target_os = "linux")]
type StorageFileBufReader<'a> = buffered_reader::SequentialFileReader<'a>;
#[cfg(not(target_os = "linux"))]
type StorageFileBufReader<'a> = buffered_reader::BufferedReader<'a, READER_STACK_BUFFER_SIZE>;

/// When `use_page_cache` is `true`, direct I/O is forced off regardless of
/// `io_setup.use_direct_io` so that reads can hit the kernel's page cache.
/// Otherwise, the `io_setup.use_direct_io` setting is honored.
pub fn storage_file_buf_reader<'a>(
    max_buf_size: usize,
    use_page_cache: bool,
    io_setup: &IoSetupState,
) -> io::Result<StorageFileBufReader<'a>> {
    #[cfg(target_os = "linux")]
    {
        buffered_reader::SequentialFileReaderBuilder::new()
            .shared_sqpoll(io_setup.shared_sqpoll_fd())
            .use_direct_io(io_setup.use_direct_io && !use_page_cache)
            .use_registered_buffers(io_setup.use_registered_io_uring_buffers)
            .build(max_buf_size)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (max_buf_size, use_page_cache, io_setup);
        Ok(StorageFileBufReader::new())
    }
}

/// Lazy iterator yielding a file handle for each storage suitable for
/// archive-style reads matching `use_direct_io` (see [`OpenFileForArchive`]).
pub fn open_storage_files<'s>(
    storages: impl IntoIterator<Item = &'s AccountStorageEntry> + 's,
    use_direct_io: bool,
) -> impl Iterator<Item = io::Result<OpenFileForArchive<'s>>> + 's {
    storages
        .into_iter()
        .map(move |storage| storage.accounts.open_file_for_archive(use_direct_io))
}

/// A wrapper type around `AccountStorageEntry` that implements the `Read` trait.
/// This type skips over the data in accounts contained in the obsolete accounts
/// structure.
///
/// The caller is responsible for activating the storage's file on `file_reader`
/// via `set_file` (typically using a file opened with [`open_storage_files`])
/// before constructing the reader.
pub struct AccountStorageReader<'r, R> {
    sorted_obsolete_accounts: Vec<(Offset, usize)>,
    reader: &'r mut R,
    num_alive_bytes: usize,
    num_total_bytes: usize,
}

impl<'a, 'r, R: FileBufRead<'a>> AccountStorageReader<'r, R> {
    /// Creates a new `AccountStorageReader` from an `AccountStorageEntry`.
    /// The obsolete accounts structure is sorted during initialization.
    ///
    /// Expects that the caller has already attached the storage's file to
    /// `file_reader` via `set_file`.
    pub fn new(
        storage: &AccountStorageEntry,
        snapshot_slot: Option<Slot>,
        file_reader: &'r mut R,
    ) -> io::Result<Self> {
        let num_total_bytes = storage.accounts.len();
        let num_alive_bytes = num_total_bytes - storage.get_obsolete_bytes(snapshot_slot);

        let mut sorted_obsolete_accounts: Vec<_> = storage
            .obsolete_accounts_read_lock()
            .filter_obsolete_accounts(snapshot_slot)
            .collect();

        // Convert the length to the size
        sorted_obsolete_accounts
            .iter_mut()
            .for_each(|(_offset, len)| {
                *len = storage.accounts.calculate_stored_size(*len);
            });

        sorted_obsolete_accounts
            .sort_unstable_by(|(a_offset, _), (b_offset, _)| b_offset.cmp(a_offset));

        Ok(Self {
            sorted_obsolete_accounts,
            reader: file_reader,
            num_alive_bytes,
            num_total_bytes,
        })
    }

    pub fn len(&self) -> usize {
        self.num_alive_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<'a, R: FileBufRead<'a>> Read for AccountStorageReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut total_read = 0;
        let buf_len = buf.len();

        while total_read < buf_len {
            let next_obsolete_account = self.sorted_obsolete_accounts.last();
            let file_offset = self.reader.get_file_offset() as usize;
            if let Some(&(obsolete_start, obsolete_size)) = next_obsolete_account {
                if file_offset == obsolete_start {
                    let skip_len = obsolete_size.min(self.num_total_bytes - obsolete_start);
                    self.reader.consume_or_skip(skip_len);
                    self.sorted_obsolete_accounts.pop();
                    continue;
                }
            }

            // Cannot read beyond the end of the buffer
            let bytes_left_in_buffer = buf_len.saturating_sub(total_read);

            // Cannot read beyond the next obsolete account or the end of the file
            let bytes_to_read_from_file = if let Some((obsolete_start, _)) = next_obsolete_account {
                obsolete_start.saturating_sub(file_offset)
            } else {
                self.num_total_bytes.saturating_sub(file_offset)
            };

            let bytes_to_read = bytes_left_in_buffer.min(bytes_to_read_from_file);

            let read_size = self.reader.read(&mut buf[total_read..][..bytes_to_read])?;

            if read_size == 0 {
                break; // EOF
            }

            total_read += read_size;
        }

        Ok(total_read)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            ObsoleteAccounts,
            account_storage_entry::AccountStorageEntry,
            accounts_db::get_temp_accounts_paths,
            accounts_file::{AccountsFile, AccountsFileProvider},
        },
        agave_fs::io_setup::IoSetupState,
        log::*,
        rand::{
            SeedableRng,
            rngs::StdRng,
            seq::{IndexedMutRandom as _, IndexedRandom},
        },
        solana_account::AccountSharedData,
        solana_pubkey::Pubkey,
        std::{fs::File, iter},
        test_case::test_case,
    };

    fn create_storage_for_storage_reader(
        slot: Slot,
        provider: AccountsFileProvider,
    ) -> (AccountStorageEntry, Vec<tempfile::TempDir>) {
        let id = 0;
        let (temp_dirs, paths) = get_temp_accounts_paths(1).unwrap();
        let file_size = 1024 * 1024;
        (
            AccountStorageEntry::new(&paths[0], slot, id, file_size, provider),
            temp_dirs,
        )
    }

    #[test_case(AccountsFileProvider::AppendVec)]
    fn test_account_storage_reader_no_obsolete_accounts(provider: AccountsFileProvider) {
        let (storage, _temp_dirs) = create_storage_for_storage_reader(0, provider);

        let account = AccountSharedData::new(1, 10, &Pubkey::default());
        let account2 = AccountSharedData::new(1, 10, &Pubkey::default());
        let slot = 0;

        let accounts = [
            (&Pubkey::new_unique(), &account),
            (&Pubkey::new_unique(), &account2),
        ];

        storage.accounts.write_accounts(&(slot, &accounts[..]), 0);

        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut buf_reader = storage_file_buf_reader(
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE,
            false,
            &IoSetupState::default(),
        )
        .unwrap();
        buf_reader
            .set_file(files[0].as_ref(), storage.accounts.len() as u64)
            .unwrap();
        let reader = AccountStorageReader::new(&storage, None, &mut buf_reader).unwrap();
        assert_eq!(reader.len(), storage.accounts.len());
    }

    #[test_case(0, 0)]
    #[test_case(1, 0)]
    #[test_case(1, 1)]
    #[test_case(100, 0)]
    #[test_case(100, 10)]
    #[test_case(100, 100)]
    fn test_account_storage_reader_with_obsolete_accounts(
        total_accounts: usize,
        number_of_accounts_to_remove: usize,
    ) {
        agave_logger::setup();
        let (storage, _temp_dirs) =
            create_storage_for_storage_reader(0, AccountsFileProvider::AppendVec);

        let slot = 0;

        // Create a bunch of accounts and add them to the storage
        let accounts: Vec<_> =
            iter::repeat_with(|| AccountSharedData::new(1, 10, &Pubkey::default()))
                .take(total_accounts)
                .collect();

        let accounts_to_append: Vec<_> = accounts
            .into_iter()
            .map(|account| (Pubkey::new_unique(), account))
            .collect();

        let offsets = storage
            .accounts
            .write_accounts(&(slot, &accounts_to_append[..]), 0);

        // Generate a seed from entropy and log the original seed
        let seed: u64 = rand::random();
        info!("Generated seed: {seed}");

        // Use a seedable RNG with the generated seed for reproducibility
        let mut rng = StdRng::seed_from_u64(seed);

        let obsolete_account_offset = offsets
            .map(|offsets| {
                offsets
                    .offsets
                    .choose_multiple(&mut rng, number_of_accounts_to_remove)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        assert_eq!(obsolete_account_offset.len(), number_of_accounts_to_remove);

        // Mark the obsolete accounts in storage
        let data_lens = storage
            .accounts
            .get_account_data_lens(&obsolete_account_offset);
        storage
            .obsolete_accounts()
            .write()
            .unwrap()
            .mark_accounts_obsolete(obsolete_account_offset.into_iter().zip(data_lens), 0);

        let storage = storage.reopen_as_readonly().unwrap_or(storage);

        // Create the reader and check the length
        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut file_reader = storage_file_buf_reader(
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE,
            false,
            &IoSetupState::default(),
        )
        .unwrap();
        file_reader
            .set_file(files[0].as_ref(), storage.accounts.len() as u64)
            .unwrap();
        let mut reader = AccountStorageReader::new(&storage, None, &mut file_reader).unwrap();
        let current_len = storage.accounts.len() - storage.get_obsolete_bytes(None);
        assert_eq!(reader.len(), current_len);

        // Create a temporary directory and a file within it
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file_path = temp_dir.path().join("output_file");
        let mut output_file = File::create(&temp_file_path).unwrap();

        let bytes_written = io::copy(&mut reader, &mut output_file).unwrap();
        assert_eq!(bytes_written as usize, reader.len());

        // Close the file
        drop(output_file);

        // If the number of accounts left is not zero, create a new AccountsFile from the output file
        // and verify that the number of accounts in the new file is correct
        if (total_accounts - number_of_accounts_to_remove) != 0 {
            let (accounts_file, num_accounts) =
                AccountsFile::new_from_file(temp_file_path, current_len).unwrap();

            // Verify that the correct number of accounts were found in the file
            assert_eq!(
                num_accounts,
                (total_accounts - number_of_accounts_to_remove)
            );

            // Create a new AccountStorageEntry from the output file
            let new_storage = AccountStorageEntry::new_existing(
                slot,
                0,
                accounts_file,
                ObsoleteAccounts::default(),
            );

            // Verify that the new storage has the same length as the reader
            assert_eq!(new_storage.accounts.len(), reader.len());
        }
    }

    #[test]
    fn test_account_storage_reader_filter_by_slot() {
        let (storage, _temp_dirs) =
            create_storage_for_storage_reader(10, AccountsFileProvider::AppendVec);
        let total_accounts = 30;

        let slot = 0;

        // Create a bunch of accounts and add them to the storage
        let accounts: Vec<_> =
            iter::repeat_with(|| AccountSharedData::new(1, 10, &Pubkey::default()))
                .take(total_accounts)
                .collect();

        let accounts_to_append: Vec<_> = accounts
            .into_iter()
            .map(|account| (Pubkey::new_unique(), account))
            .collect();

        let offsets = storage
            .accounts
            .write_accounts(&(slot, &accounts_to_append[..]), 0);

        // Generate a seed from entropy and log the original seed
        let seed: u64 = rand::random();
        info!("Generated seed: {seed}");

        // Use a seedable RNG with the generated seed for reproducibility
        let mut rng = StdRng::seed_from_u64(seed);

        let max_offset = offsets
            .as_ref()
            .and_then(|offsets| offsets.offsets.iter().max().cloned())
            .unwrap();

        let mut obsolete_account_offset = offsets
            .map(|offsets| {
                offsets
                    .offsets
                    .choose_multiple(&mut rng, total_accounts - 1)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Ensure that the last entry will be marked obsolete at some point
        if !obsolete_account_offset.contains(&max_offset) {
            // Replace a random obsolete account with the max offset
            if let Some(random_index) = obsolete_account_offset.choose_mut(&mut rng) {
                *random_index = max_offset;
            }
        }

        // Mark the obsolete accounts in storage at different slots
        let mut slot_marked_dead = 0;
        obsolete_account_offset.into_iter().for_each(|offset| {
            let mut size = storage.accounts.get_account_data_lens(&[offset]);
            storage
                .obsolete_accounts()
                .write()
                .unwrap()
                .mark_accounts_obsolete(
                    vec![(offset, size.pop().unwrap())].into_iter(),
                    slot_marked_dead,
                );
            slot_marked_dead += 1;
        });

        // Create a temporary directory
        let temp_dir = tempfile::tempdir().unwrap();

        // Now iterate through all the possible snapshot slots and verify correctness
        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut file_reader = storage_file_buf_reader(
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE,
            false,
            &IoSetupState::default(),
        )
        .unwrap();
        for snapshot_slot in 0..slot_marked_dead {
            file_reader
                .set_file(files[0].as_ref(), storage.accounts.len() as u64)
                .unwrap();
            let mut reader =
                AccountStorageReader::new(&storage, Some(snapshot_slot), &mut file_reader).unwrap();
            let current_len =
                storage.accounts.len() - storage.get_obsolete_bytes(Some(snapshot_slot));
            assert_eq!(reader.len(), current_len);

            // Create a file to write the reader's output. It will get deleted by AccountsFile::drop() every
            // iteration so it does not need a unique name
            let temp_file_path = temp_dir.path().join("output_file");
            let mut output_file = File::create(&temp_file_path).unwrap();

            let bytes_written = io::copy(&mut reader, &mut output_file).unwrap();
            assert_eq!(bytes_written as usize, reader.len());

            // Close the file
            drop(output_file);

            let (accounts_file, _num_accounts) =
                AccountsFile::new_from_file(temp_file_path, current_len).unwrap();

            // Create a new AccountStorageEntry from the output file
            let new_storage = AccountStorageEntry::new_existing(
                slot,
                0,
                accounts_file,
                ObsoleteAccounts::default(),
            );

            // Verify that the new storage has the same length as the reader
            assert_eq!(new_storage.accounts.len(), reader.len());
        }
    }
}
