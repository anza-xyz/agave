use {
    crate::{
        account_info::Offset,
        account_storage_entry::AccountStorageEntry,
        accounts_file::OpenFileForArchive,
        append_vec::{AppendVec, AppendVecAccountWriter},
    },
    agave_fs::{
        buffered_reader::{self, BufReaderWithOverflow, RequiredLenBufFileRead},
        io_setup::IoSetupState,
    },
    solana_clock::Slot,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    std::{
        cmp,
        io::{self, Write},
    },
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
type StorageFileBufReader<'a> = BufReaderWithOverflow<buffered_reader::SequentialFileReader<'a>>;
#[cfg(not(target_os = "linux"))]
type StorageFileBufReader<'a> =
    BufReaderWithOverflow<buffered_reader::BufferedReader<'a, READER_STACK_BUFFER_SIZE>>;

/// When `use_page_cache` is `true`, direct I/O is forced off regardless of
/// `io_setup.use_direct_io` so that reads can hit the kernel's page cache.
/// Otherwise, the `io_setup.use_direct_io` setting is honored.
pub fn storage_file_buf_reader<'a>(
    max_buf_size: usize,
    use_page_cache: bool,
    io_setup: &IoSetupState,
) -> io::Result<StorageFileBufReader<'a>> {
    #[cfg(target_os = "linux")]
    let reader = {
        buffered_reader::SequentialFileReaderBuilder::new()
            .shared_sqpoll(io_setup.shared_sqpoll_fd())
            .use_direct_io(io_setup.use_direct_io && !use_page_cache)
            .use_registered_buffers(io_setup.use_registered_io_uring_buffers)
            .build(max_buf_size)?
    };
    #[cfg(not(target_os = "linux"))]
    let reader = {
        let _ = (max_buf_size, use_page_cache, io_setup);
        buffered_reader::BufferedReader::<READER_STACK_BUFFER_SIZE>::new()
    };
    // Refer to append vec/split file new_scan_accounts_reader()
    // for documentation/comments w.r.t. the minimum capacity.
    const MIN_CAPACITY: usize = 128 * 1024;
    // The max capacity needed is based on the max permitted account data size
    // plus additional space required to read the account's metadata.
    // Note that this reader must work on all underlying account storage formats,
    // and so the additional size must be >= the max metadata size of any format.
    const MAX_CAPACITY: usize = 4096 + MAX_PERMITTED_DATA_LENGTH as usize;
    Ok(BufReaderWithOverflow::new(
        reader,
        MIN_CAPACITY,
        MAX_CAPACITY,
    ))
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

/// Should tombstones be included or excluded when reading from storage?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstonesFilter {
    /// tombstones are included when reading from storage
    Include,
    /// tombstones are excluded when reading from storage
    Exclude,
}

/// A wrapper type around `AccountStorageEntry` that scans accounts into an archive writer.
/// This type skips over the data in accounts contained in the obsolete accounts
/// structure, and optionally over tombstone accounts as well.
///
/// The caller is responsible for activating the storage's file on `file_reader`
/// via `set_file` (typically using a file opened with [`open_storage_files`])
/// before constructing the reader.
pub struct AccountStorageReader<'s, 'r, R> {
    storage: &'s AccountStorageEntry,
    reader: &'r mut R,
    sorted_excluded_offsets: Vec<Offset>,
    len_for_archive: usize,
}

impl<'s, 'r, R: RequiredLenBufFileRead<'s>> AccountStorageReader<'s, 'r, R> {
    /// Creates a new `AccountStorageReader` from an `AccountStorageEntry`.
    /// The excluded accounts list is sorted during initialization.
    ///
    /// Expects that the caller has already attached the storage's file to
    /// `file_reader` via `set_file`.
    pub fn new(
        storage: &'s AccountStorageEntry,
        snapshot_slot: Option<Slot>,
        tombstones_filter: TombstonesFilter,
        file_reader: &'r mut R,
    ) -> io::Result<Self> {
        let mut excluded_accounts: Vec<_> = storage
            .obsolete_accounts_read_lock()
            .filter_obsolete_accounts(snapshot_slot)
            .collect();

        if tombstones_filter == TombstonesFilter::Exclude {
            let tombstone_offsets = storage.tombstone_offsets_read_lock();
            // Tombstones are zero-lamport accounts, which store no data.
            excluded_accounts.extend(tombstone_offsets.iter().map(|offset| (*offset, 0)));
        }

        let len_for_archive = storage.accounts.len_for_archive(
            excluded_accounts
                .iter()
                .map(|(_offset, data_len)| *data_len),
        );

        let mut excluded_offsets: Vec<_> = excluded_accounts
            .into_iter()
            .map(|(offset, _)| offset)
            .collect();
        // offsets are sorted in descending order because they are traversed in reverse order
        excluded_offsets.sort_unstable_by_key(|k| cmp::Reverse(*k));
        // ensure there are no duplicates
        debug_assert!(excluded_offsets.array_windows::<2>().all(|[a, b]| a != b));

        Ok(Self {
            storage,
            reader: file_reader,
            sorted_excluded_offsets: excluded_offsets,
            len_for_archive,
        })
    }

    /// Returns the number of bytes required to archive this AccountStorageEntry.
    ///
    /// Note that snapshot archives always use the AppendVec format, so
    /// this is effectively computing the AppendVec stored size.
    pub fn len_for_archive(&self) -> usize {
        self.len_for_archive
    }

    /// Scans accounts, skips excluded offsets, and writes AppendVec archive records.
    pub fn write_to(mut self, output: impl Write) -> io::Result<()> {
        let mut account_writer = AppendVecAccountWriter::new(output);
        let mut remaining = self.len_for_archive;
        let mut write_result = Ok(());
        let scan_result =
            self.storage
                .accounts
                .scan_accounts_with(self.reader, |offset, account| {
                    if write_result.is_err() {
                        return;
                    }
                    if self
                        .sorted_excluded_offsets
                        .pop_if(|excluded_offset| *excluded_offset == offset)
                        .is_some()
                    {
                        return;
                    }
                    let stored_size = AppendVec::calculate_stored_size(account.data.len());
                    if stored_size > remaining {
                        write_result = Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "account exceeds archive size",
                        ));
                        return;
                    }
                    write_result = account_writer.write_account(&account);
                    remaining -= stored_size;
                });
        // Preserve the original output error if a later scan also fails.
        write_result?;
        scan_result.map_err(io::Error::other)?;
        if remaining != 0 || !self.sorted_excluded_offsets.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete account archive scan",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            ObsoleteAccounts,
            account_storage_entry::AccountStorageEntry,
            accounts_file::{AccountsFile, AccountsFileProvider},
            append_vec,
            utils::create_account_shared_data,
        },
        agave_fs::{FileInfo, buffered_reader::FileBufRead as _, io_setup::IoSetupState},
        log::*,
        rand::{
            SeedableRng,
            rngs::StdRng,
            seq::{IndexedMutRandom as _, IndexedRandom},
        },
        solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
        solana_pubkey::Pubkey,
        std::{
            collections::HashMap,
            fs::{self, File},
            iter,
        },
        tempfile::TempDir,
        test_case::{test_case, test_matrix},
    };

    #[test_case(AccountsFileProvider::AppendVec)]
    fn test_account_storage_reader_no_obsolete_accounts(provider: AccountsFileProvider) {
        let slot = 0;
        let temp_dir = TempDir::new().unwrap();
        let storage = AccountStorageEntry::new(temp_dir.path(), slot, 11, 1_000_000, provider);

        let account = AccountSharedData::new(1, 10, &Pubkey::default());
        let account2 = AccountSharedData::new(1, 10, &Pubkey::default());

        let accounts = [
            (&Pubkey::new_unique(), &account),
            (&Pubkey::new_unique(), &account2),
        ];

        storage.accounts.write_accounts(&(slot, &accounts[..]));

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
        let reader =
            AccountStorageReader::new(&storage, None, TombstonesFilter::Include, &mut buf_reader)
                .unwrap();
        assert_eq!(
            reader.len_for_archive(),
            2 * AppendVec::calculate_stored_size(10)
        );
    }

    #[test_case(0, 0, 0, TombstonesFilter::Include)]
    #[test_case(1, 0, 0, TombstonesFilter::Include)]
    #[test_case(1, 1, 0, TombstonesFilter::Include)]
    #[test_case(1, 1, 0, TombstonesFilter::Exclude)]
    #[test_case(1, 0, 1, TombstonesFilter::Include)]
    #[test_case(100, 0, 0, TombstonesFilter::Include)]
    #[test_case(100, 0, 10, TombstonesFilter::Include)]
    #[test_case(100, 0, 100, TombstonesFilter::Include)]
    #[test_case(100, 10, 0, TombstonesFilter::Include)]
    #[test_case(100, 10, 0, TombstonesFilter::Exclude)]
    #[test_case(100, 100, 0, TombstonesFilter::Include)]
    #[test_case(100, 100, 0, TombstonesFilter::Exclude)]
    #[test_case(100, 10, 10, TombstonesFilter::Include)]
    #[test_case(100, 10, 10, TombstonesFilter::Exclude)]
    fn test_account_storage_reader_with_excluded_accounts(
        total_accounts: usize,
        num_tombstones: usize,
        num_obsolete: usize,
        tombstones_filter: TombstonesFilter,
    ) {
        let slot = 0;
        let temp_dir = TempDir::new().unwrap();
        let storage = AccountStorageEntry::new(
            temp_dir.path(),
            slot,
            11,
            1_000_000,
            AccountsFileProvider::AppendVec,
        );

        // Generate a seed from entropy and log the original seed
        let seed: u64 = rand::random();
        dbg!(seed);

        // Use a seedable RNG with the generated seed for reproducibility
        let mut rng = StdRng::seed_from_u64(seed);

        // Choose disjoint random index sets for the tombstone and obsolete accounts.
        // Tombstones must be chosen before writing because they are written as
        // zero-lamport, data-less accounts.
        let chosen_indexes = (0..total_accounts)
            .collect::<Vec<_>>()
            .choose_multiple(&mut rng, num_tombstones + num_obsolete)
            .cloned()
            .collect::<Vec<_>>();
        let (tombstone_indexes, obsolete_indexes) = chosen_indexes.split_at(num_tombstones);

        // Create a bunch of accounts and add them to the storage
        let accounts: Vec<_> = (0..total_accounts)
            .map(|index| {
                if tombstone_indexes.contains(&index) {
                    AccountSharedData::new(0, 0, &Pubkey::default())
                } else {
                    AccountSharedData::new(1, 10, &Pubkey::default())
                }
            })
            .collect();

        let accounts_to_append: Vec<_> = accounts
            .into_iter()
            .map(|account| (Pubkey::new_unique(), account))
            .collect();

        let offsets = storage
            .accounts
            .write_accounts(&(slot, &accounts_to_append[..]))
            .map(|stored_accounts_info| stored_accounts_info.offsets)
            .unwrap_or_default();

        let tombstone_offsets: Vec<_> = tombstone_indexes
            .iter()
            .map(|index| offsets[*index])
            .collect();
        let obsolete_offsets: Vec<_> = obsolete_indexes
            .iter()
            .map(|index| offsets[*index])
            .collect();

        storage.batch_insert_tombstone_offsets(tombstone_offsets);

        // Mark the obsolete accounts in storage
        let data_lens = storage
            .accounts
            .get_account_data_lens(obsolete_offsets.iter().copied());
        storage
            .obsolete_accounts()
            .write()
            .unwrap()
            .mark_accounts_obsolete(obsolete_offsets.iter().copied().zip(data_lens), 0);

        let storage = storage.reopen_as_readonly().unwrap().unwrap_or(storage);

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
        let reader =
            AccountStorageReader::new(&storage, None, tombstones_filter, &mut file_reader).unwrap();
        let mut number_of_accounts_to_remove = num_obsolete;
        if tombstones_filter == TombstonesFilter::Exclude {
            number_of_accounts_to_remove += num_tombstones;
        }

        // Create a temporary directory and a file within it
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file_path = temp_dir.path().join("output_file");
        let mut output_file = File::create(&temp_file_path).unwrap();

        let reader_len = reader.len_for_archive();
        reader.write_to(&mut output_file).unwrap();
        assert_eq!(output_file.metadata().unwrap().len(), reader_len as u64);

        // Close the file
        drop(output_file);

        // If the number of accounts left is not zero, create a new AccountsFile from the output file
        // and verify that the number of accounts in the new file is correct
        if (total_accounts - number_of_accounts_to_remove) != 0 {
            let file_info = FileInfo::new_from_path(temp_file_path).unwrap();
            let accounts_file = AccountsFile::new_for_startup(file_info).unwrap();

            // Verify that the correct number of accounts were found in the file
            let mut num_accounts = 0;
            accounts_file.scan_pubkeys(|_| num_accounts += 1).unwrap();
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
            assert_eq!(new_storage.accounts.len(), reader_len);

            // Verify that the new storage has all the expected accounts
            let include_tombstones = tombstones_filter == TombstonesFilter::Include;
            let expected_accounts: HashMap<_, _> = accounts_to_append
                .iter()
                .enumerate()
                .filter_map(|(i, (pubkey, account))| {
                    let is_obsolete = obsolete_indexes.contains(&i);
                    let is_tombstone = tombstone_indexes.contains(&i);
                    (!is_obsolete && (!is_tombstone || include_tombstones))
                        .then(|| (*pubkey, account.clone()))
                })
                .collect();
            let mut reader_for_scan_accounts = append_vec::new_scan_accounts_reader();
            let accounts_in_new_storage = {
                let mut accounts = HashMap::new();
                new_storage
                    .accounts
                    .scan_accounts(&mut reader_for_scan_accounts, |_offset, stored_account| {
                        let old_value = accounts.insert(
                            *stored_account.pubkey(),
                            create_account_shared_data(&stored_account),
                        );
                        assert!(old_value.is_none());
                    })
                    .unwrap();
                accounts
            };
            assert_eq!(accounts_in_new_storage, expected_accounts);
        }
    }

    #[test]
    fn test_account_storage_reader_filter_by_slot() {
        let slot = 0;
        let temp_dir = TempDir::new().unwrap();
        let storage = AccountStorageEntry::new(
            temp_dir.path(),
            slot,
            11,
            1_000_000,
            AccountsFileProvider::AppendVec,
        );
        let total_accounts = 30;

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
            .write_accounts(&(slot, &accounts_to_append[..]));

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
            let mut size = storage.accounts.get_account_data_lens([offset]);
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
            let obsolete_slot = Some(snapshot_slot);
            file_reader
                .set_file(files[0].as_ref(), storage.accounts.len() as u64)
                .unwrap();
            let reader = AccountStorageReader::new(
                &storage,
                obsolete_slot,
                TombstonesFilter::Include,
                &mut file_reader,
            )
            .unwrap();

            // Create a file to write the reader's output. It will get deleted by AccountsFile::drop() every
            // iteration so it does not need a unique name
            let temp_file_path = temp_dir.path().join("output_file");
            let mut output_file = File::create(&temp_file_path).unwrap();

            let reader_len = reader.len_for_archive();
            reader.write_to(&mut output_file).unwrap();
            assert_eq!(output_file.metadata().unwrap().len(), reader_len as u64);

            // Close the file
            drop(output_file);

            // Create a new AccountStorageEntry from the output file
            let file_info = FileInfo::new_from_path(temp_file_path).unwrap();
            let accounts_file = AccountsFile::new_for_startup(file_info).unwrap();
            let new_storage = AccountStorageEntry::new_existing(
                slot,
                0,
                accounts_file,
                ObsoleteAccounts::default(),
            );

            // Verify that the new storage has the same length as the reader
            assert_eq!(new_storage.accounts.len(), reader_len);

            // Verify that the new storage has all the expected accounts
            let mut reader_for_scan_accounts = append_vec::new_scan_accounts_reader();
            let accounts_in_old_storage = {
                let mut accounts = HashMap::new();
                storage
                    .scan_accounts(
                        &mut reader_for_scan_accounts,
                        obsolete_slot,
                        |_offset, stored_account| {
                            let old_value = accounts.insert(
                                *stored_account.pubkey(),
                                create_account_shared_data(&stored_account),
                            );
                            assert!(old_value.is_none());
                        },
                    )
                    .unwrap();
                accounts
            };
            let accounts_in_new_storage = {
                let mut accounts = HashMap::new();
                new_storage
                    .accounts
                    .scan_accounts(&mut reader_for_scan_accounts, |_offset, stored_account| {
                        let old_value = accounts.insert(
                            *stored_account.pubkey(),
                            create_account_shared_data(&stored_account),
                        );
                        assert!(old_value.is_none());
                    })
                    .unwrap();
                accounts
            };
            assert_eq!(accounts_in_new_storage, accounts_in_old_storage);
        }
    }

    /// Tests that AccountStoredReader::write_to() handles:
    /// * writing the accounts in correct AppendVec format
    /// * writing padding for alignment
    /// * excluded accounts
    /// * exceeding the file reader's stack buffer
    #[test_matrix(
        [false, true],
        [0, 1, 2, 3, 4, 5, 6, 7])
    ]
    fn test_write_to(exclude_last_account: bool, data_len_last_account: usize) {
        let slot = 11;
        let temp_dir = TempDir::new().unwrap();
        let storage = AccountStorageEntry::new(
            temp_dir.path(),
            slot,
            11,
            1_000_000,
            AccountsFileProvider::AppendVec,
        );
        let accounts: Vec<_> = [3, 256 * 1024 + 1, data_len_last_account]
            .into_iter()
            .enumerate()
            .map(|(i, data_len)| {
                let mut account =
                    AccountSharedData::new(100 + i as u64, data_len, &Pubkey::new_unique());
                account.set_data_from_slice(&vec![i as u8 + 1; data_len]);
                account.set_executable(i == 1);
                account.set_rent_epoch(42 + i as u64);
                (Pubkey::new_unique(), account)
            })
            .collect();
        let stored_accounts_info = storage
            .accounts
            .write_accounts(&(0, &accounts[..]))
            .unwrap();
        // exclude one account, either the first or last
        let excluded_index = if exclude_last_account { 2 } else { 0 };
        storage
            .obsolete_accounts()
            .write()
            .unwrap()
            .mark_accounts_obsolete(
                [(
                    stored_accounts_info.offsets[excluded_index],
                    accounts[excluded_index].1.data().len(),
                )]
                .into_iter(),
                slot,
            );
        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut file_reader = BufReaderWithOverflow::new(
            // using small 64 byte stack buffer here to cause all reads
            // to use the scanner's overflow buffer
            buffered_reader::BufferedReader::<64>::new(),
            128 * 1024,
            4096 + MAX_PERMITTED_DATA_LENGTH as usize,
        );
        file_reader
            .set_file(files[0].as_ref(), storage.accounts.len() as u64)
            .unwrap();
        let storage_reader =
            AccountStorageReader::new(&storage, None, TombstonesFilter::Include, &mut file_reader)
                .unwrap();
        let archive_len = storage_reader.len_for_archive();
        let mut output_buf = Vec::new();
        storage_reader.write_to(&mut output_buf).unwrap();
        assert_eq!(output_buf.len(), archive_len);
        let included_accounts: Vec<_> = accounts
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != excluded_index)
            .collect();
        let expected_len: usize = included_accounts
            .iter()
            .map(|(_i, (_pubkey, account))| AppendVec::calculate_stored_size(account.data().len()))
            .sum();
        assert_eq!(archive_len, expected_len);

        let archive_path = temp_dir.path().join("archive");
        fs::write(&archive_path, output_buf).unwrap();

        let archived_file_info = FileInfo::new_from_path(&archive_path).unwrap();
        let archived_storage = AccountsFile::new_for_startup(archived_file_info).unwrap();
        let mut archived_num_accounts = 0;
        let mut expected_accounts_iter = included_accounts.iter();
        archived_storage
            .scan_accounts(&mut append_vec::new_scan_accounts_reader(), |_, account| {
                let (_, (pubkey, original)) = expected_accounts_iter.next().unwrap();
                assert_eq!(account.pubkey, pubkey);
                assert_eq!(account.lamports, original.lamports());
                assert_eq!(account.owner, original.owner());
                assert_eq!(account.data, original.data());
                assert_eq!(account.executable, original.executable());
                assert_eq!(account.rent_epoch, original.rent_epoch());
                archived_num_accounts += 1;
            })
            .unwrap();
        assert!(expected_accounts_iter.next().is_none());
        assert_eq!(archived_num_accounts, included_accounts.len());
    }
}
