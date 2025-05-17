use {
    crate::{
        account_info::Offset, accounts_db::AccountStorageEntry, accounts_file::InternalsForArchive,
    },
    solana_clock::Slot,
    std::{
        fs::File,
        io::{self, Read, Seek, SeekFrom},
    },
};

/// A wrapper type around `AccountStorageEntry` that implements the `Read` trait.
/// This type skips over the data in the sorted dead accounts structure.
pub struct AccountStorageReader<'a> {
    sorted_dead_accounts: Vec<(Offset, usize)>,
    current_offset: usize,
    file: Option<File>,
    internals: InternalsForArchive<'a>,
    num_alive_bytes: usize,
    num_total_bytes: usize,
}

impl<'a> AccountStorageReader<'a> {
    /// Creates a new `AccountStorageReader` from an `AccountStorageEntry`.
    /// The dead accounts structure is sorted during initialization.
    pub fn new(storage: &'a AccountStorageEntry, snapshot_slot: Option<Slot>) -> io::Result<Self> {
        let internals = storage.accounts.internals_for_archive();
        let num_total_bytes = storage.accounts.len();
        let num_alive_bytes = num_total_bytes - storage.get_dead_account_bytes(snapshot_slot);

        let mut sorted_dead_accounts = storage.get_dead_accounts(snapshot_slot);

        // Conver the length to the size
        sorted_dead_accounts.iter_mut().for_each(|(_offset, len)| {
            *len = storage.accounts.calculate_stored_size(*len);
        });

        sorted_dead_accounts
            .sort_unstable_by(|(a_offset, _), (b_offset, _)| b_offset.cmp(a_offset));

        let file = match internals {
            InternalsForArchive::Mmap(_internals) => None,
            InternalsForArchive::FileIo(path) => Some(File::open(path)?),
        };

        Ok(Self {
            sorted_dead_accounts,
            current_offset: 0,
            file,
            internals,
            num_alive_bytes,
            num_total_bytes,
        })
    }

    pub fn len(&self) -> usize {
        self.num_alive_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.num_alive_bytes == 0
    }
}

impl Read for AccountStorageReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut total_read = 0;
        let buf_len = buf.len();

        while total_read < buf_len {
            let next_dead_account = self.sorted_dead_accounts.last();
            if let Some(&(dead_start, dead_size)) = next_dead_account {
                if self.current_offset == dead_start {
                    self.current_offset += dead_size.min(self.num_total_bytes - dead_start);
                    self.sorted_dead_accounts.pop();
                    continue;
                }
            }

            // Cannot read beyond the end of the buffer
            let bytes_left_in_buffer = buf_len.saturating_sub(total_read);

            // Cannot read beyond the next dead account or the end of the file
            let bytes_to_read_from_file = if let Some(&(dead_start, _)) = next_dead_account {
                dead_start.saturating_sub(self.current_offset)
            } else {
                self.num_total_bytes.saturating_sub(self.current_offset)
            };

            let bytes_to_read = bytes_left_in_buffer.min(bytes_to_read_from_file);

            let read_size = match self.internals {
                InternalsForArchive::Mmap(data) => (&data
                    [self.current_offset..self.current_offset + bytes_to_read])
                    .read(&mut buf[total_read..][..bytes_to_read])?,

                InternalsForArchive::FileIo(_) => {
                    let file = &mut self
                        .file
                        .as_mut()
                        .expect("File is opened during initialization");
                    file.seek(SeekFrom::Start(self.current_offset as u64))?;
                    file.read(&mut buf[total_read..][..bytes_to_read])?
                }
            };

            if read_size == 0 {
                break; // EOF
            }

            self.current_offset += read_size;
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
            accounts_db::{get_temp_accounts_paths, AccountStorageEntry},
            accounts_file::{AccountsFile, AccountsFileProvider, StorageAccess},
        },
        solana_account::AccountSharedData,
        solana_pubkey::Pubkey,
        std::iter,
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
    #[test_case(AccountsFileProvider::HotStorage)]
    fn test_account_storage_reader_no_dead_accounts(provider: AccountsFileProvider) {
        let (storage, _temp_dirs) = create_storage_for_storage_reader(0, provider);

        let account = AccountSharedData::new(1, 10, &Pubkey::new_unique());
        let account2 = AccountSharedData::new(1, 10, &Pubkey::new_unique());
        let shared_key = solana_pubkey::new_rand();
        let slot = 0;

        let accounts = [(&shared_key, &account), (&shared_key, &account2)];

        storage.accounts.append_accounts(&(slot, &accounts[..]), 0);

        let reader = AccountStorageReader::new(&storage, None).unwrap();
        assert_eq!(reader.len(), storage.accounts.len());
    }

    #[test_case(0, 0, StorageAccess::File)]
    #[test_case(1, 0, StorageAccess::File)]
    #[test_case(1, 1, StorageAccess::File)]
    #[test_case(100, 0, StorageAccess::File)]
    #[test_case(100, 10, StorageAccess::File)]
    #[test_case(100, 100, StorageAccess::File)]
    #[test_case(0, 0, StorageAccess::Mmap)]
    #[test_case(1, 0, StorageAccess::Mmap)]
    #[test_case(1, 1, StorageAccess::Mmap)]
    #[test_case(100, 0, StorageAccess::Mmap)]
    #[test_case(100, 10, StorageAccess::Mmap)]
    #[test_case(100, 100, StorageAccess::Mmap)]
    fn test_account_storage_reader_with_dead_accounts(
        total_accounts: usize,
        number_of_accounts_to_remove: usize,
        storage_access: StorageAccess,
    ) {
        let (storage, _temp_dirs) =
            create_storage_for_storage_reader(0, AccountsFileProvider::AppendVec);

        let shared_key = solana_pubkey::new_rand();
        let slot = 0;

        // Create a bunch of accounts and add them to the storage
        let accounts: Vec<_> =
            iter::repeat_with(|| AccountSharedData::new(1, 10, &Pubkey::new_unique()))
                .take(total_accounts)
                .collect();

        let accounts_to_append: Vec<_> = accounts
            .iter()
            .map(|account| (&shared_key, account))
            .collect();

        let offsets = storage
            .accounts
            .append_accounts(&(slot, &accounts_to_append[..]), 0);

        // Select some accounts to mark as dead.
        let mut dead_account_offset = Vec::new();

        // Offsets may be None if the storage is empty
        if let Some(offsets) = offsets {
            offsets.offsets.iter().enumerate().for_each(|(i, offset)| {
                // Remove the specified percentage of accounts
                if (number_of_accounts_to_remove != 0)
                    && (i % (total_accounts / number_of_accounts_to_remove)) == 0
                {
                    dead_account_offset.push(*offset);
                }
            })
        };

        // Reopen the storage as the specified access type
        storage.reopen_as_readonly_test_hook(storage_access);

        assert_eq!(dead_account_offset.len(), number_of_accounts_to_remove);

        // Mark the dead accounts in storage
        dead_account_offset.into_iter().for_each(|offset| {
            let mut size = storage.accounts.get_account_data_lens(&[offset]);
            storage.add_dead_account(offset, size.pop().unwrap(), 0);
        });

        // Create the reader and check the length
        let mut reader = AccountStorageReader::new(&storage, None).unwrap();
        let current_len = storage.accounts.len() - storage.get_dead_account_bytes(None);
        assert_eq!(reader.len(), current_len);

        // Create a temporary directory and a file within it
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file_path = temp_dir.path().join("output_file");
        let mut output_file = File::create(&temp_file_path).unwrap();

        let bytes_written = std::io::copy(&mut reader, &mut output_file).unwrap();
        assert_eq!(bytes_written as usize, reader.len());

        // Close the file
        drop(output_file);

        // If the number of accounts left is not zero, create a new AccountsFile from the output file
        // and verify that the number of accounts in the new file is correct
        if (total_accounts - number_of_accounts_to_remove) != 0 {
            let (accounts_file, num_accounts) =
                AccountsFile::new_from_file(temp_file_path, current_len, StorageAccess::File)
                    .unwrap();

            // Verify that the correct numbe rof accounts were found in the file
            assert_eq!(
                num_accounts,
                (total_accounts - number_of_accounts_to_remove)
            );

            // Create a new AccountStorageEntry from the output file
            let new_storage =
                AccountStorageEntry::new_existing(slot, 0, accounts_file, num_accounts);

            // Verify that the new storage has the same length as the reader
            assert_eq!(new_storage.accounts.len(), reader.len());
        }
    }

    #[test]
    fn test_account_storage_reader_filter_by_slot() {
        let (storage, _temp_dirs) =
            create_storage_for_storage_reader(10, AccountsFileProvider::AppendVec);

        let shared_key = solana_pubkey::new_rand();
        let slot = 10;

        // Create a bunch of accounts and add them to the storage
        let accounts: Vec<_> =
            iter::repeat_with(|| AccountSharedData::new(1, 10, &Pubkey::new_unique()))
                .take(30)
                .collect();

        let accounts_to_append: Vec<_> = accounts
            .iter()
            .map(|account| (&shared_key, account))
            .collect();

        let offsets = storage
            .accounts
            .append_accounts(&(slot, &accounts_to_append[..]), 0);

        // Select some accounts to mark as dead. Need to find the starting offset of the account to mark them
        let mut dead_account_offset = Vec::new();

        offsets
            .unwrap()
            .offsets
            .iter()
            .enumerate()
            .for_each(|(i, offset)| {
                // Mark half the accounts as dead
                if i % 2 == 0 {
                    dead_account_offset.push(*offset);
                }
            });

        // Mark the dead accounts in storage
        let mut slot = 0;
        dead_account_offset.into_iter().for_each(|offset| {
            let mut size = storage.accounts.get_account_data_lens(&[offset]);
            storage.add_dead_account(offset, size.pop().unwrap(), slot);
            slot += 1;
        });

        // Create the reader and check the length
        // Now iterate through all the slots and verify correctness
        for slot in 1..=10 {
            let mut reader = AccountStorageReader::new(&storage, Some(slot)).unwrap();
            let current_len = storage.accounts.len() - storage.get_dead_account_bytes(Some(slot));
            assert_eq!(reader.len(), current_len);

            // Create a temporary directory and a file within it
            let temp_dir = tempfile::tempdir().unwrap();
            let temp_file_path = temp_dir.path().join("output_file");
            let mut output_file = File::create(&temp_file_path).unwrap();

            let bytes_written = std::io::copy(&mut reader, &mut output_file).unwrap();
            assert_eq!(bytes_written as usize, reader.len());

            // Close the file
            drop(output_file);

            let (accounts_file, num_accounts) =
                AccountsFile::new_from_file(temp_file_path, current_len, StorageAccess::File)
                    .unwrap();

            // Create a new AccountStorageEntry from the output file
            let new_storage =
                AccountStorageEntry::new_existing(slot, 0, accounts_file, num_accounts);

            // Verify that the new storage has the same length as the reader
            assert_eq!(new_storage.accounts.len(), reader.len());
        }
    }
}
