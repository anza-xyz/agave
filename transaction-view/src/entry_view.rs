use {
    crate::{
        bytes::check_remaining, result::Result, transaction_data::TransactionData,
        transaction_frame::TransactionFrame, transaction_view::UnsanitizedTransactionView,
    },
    bytes::{Buf, Bytes},
    solana_sdk::hash::Hash,
};

pub trait EntryData: Clone {
    /// Associated type for the transaction data.
    type Tx: TransactionData;

    /// Get the remaining data slice for the entry.
    fn data(&self) -> &[u8];

    /// Move offset forward `num_bytes`.
    fn advance(&mut self, num_bytes: usize);

    /// Split the a `TransactionData` off the front of the entry.
    fn split_to(&mut self, index: usize) -> Self::Tx;
}

/// Reads an entry from the provided `data`.
/// The `handle_transaction` closure is called for each transaction in the
/// entry.
/// Returns the number of hashes, the hash, and the number of transactions in
/// the entry.
/// If the entry, or any transactions within the entry, are invalid, an error
/// is returned.
pub fn read_entry<D: EntryData>(
    data: &mut D,
    mut handle_transaction: impl FnMut(UnsanitizedTransactionView<D::Tx>),
) -> Result<(u64, Hash, u64)> {
    let (num_hashes, hash, num_transactions) = read_entry_header(data)?;
    for _ in 0..num_transactions {
        let transaction = read_transaction_view(data)?;
        handle_transaction(transaction);
    }

    Ok((num_hashes, hash, num_transactions))
}

/// Read the header information from an entry.
/// This includes:
/// - `num_hashes` - u64
/// - `hash` - Hash
/// - `num_transactions` - u64
#[inline]
fn read_entry_header(entry_data: &mut impl EntryData) -> Result<(u64, Hash, u64)> {
    const HEADER_SIZE: usize =
        core::mem::size_of::<u64>() + core::mem::size_of::<Hash>() + core::mem::size_of::<u64>();

    // Check that there are enough bytes to read the header.
    let bytes = entry_data.data();
    check_remaining(bytes, 0, HEADER_SIZE)?;

    // Read the header without checking bounds.
    let (num_hashes, hash, num_transactions) = unsafe {
        let ptr = bytes.as_ptr();
        let num_hashes = u64::from_le_bytes(ptr.cast::<[u8; 8]>().read_unaligned());
        let hash = ptr
            .add(core::mem::size_of::<u64>())
            .cast::<Hash>()
            .read_unaligned();
        let num_transactions = u64::from_le_bytes(
            ptr.add(core::mem::size_of::<u64>().wrapping_add(core::mem::size_of::<Hash>()))
                .cast::<[u8; 8]>()
                .read_unaligned(),
        );
        (num_hashes, hash, num_transactions)
    };
    entry_data.advance(HEADER_SIZE);

    Ok((num_hashes, hash, num_transactions))
}

#[inline]
fn read_transaction_view<D: EntryData>(data: &mut D) -> Result<UnsanitizedTransactionView<D::Tx>> {
    let (frame, bytes_read) = TransactionFrame::try_new(data.data())?;
    let transaction_data = data.split_to(bytes_read);
    // SAFETY: The format was verified by `TransactionFrame::try_new`.
    let view = unsafe { UnsanitizedTransactionView::from_data_unchecked(transaction_data, frame) };
    Ok(view)
}

// Implement `EntryData` for `Bytes`.
impl EntryData for Bytes {
    type Tx = Bytes;

    #[inline]
    fn data(&self) -> &[u8] {
        self
    }

    #[inline]
    fn advance(&mut self, index: usize) {
        Buf::advance(self, index);
    }

    #[inline]
    fn split_to(&mut self, index: usize) -> Self::Tx {
        Bytes::split_to(self, index)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        bytes::BufMut,
        solana_entry::entry::Entry,
        solana_sdk::{
            message::{Message, VersionedMessage},
            pubkey::Pubkey,
            signature::Signature,
            system_instruction,
            transaction::VersionedTransaction,
        },
    };

    fn simple_transfer() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::Legacy(Message::new(
                &[system_instruction::transfer(
                    &payer,
                    &Pubkey::new_unique(),
                    1,
                )],
                Some(&payer),
            )),
        }
    }

    fn create_entry_and_serialize(transactions: Vec<VersionedTransaction>) -> (Entry, Bytes) {
        let entry = Entry {
            num_hashes: 42,
            hash: Hash::default(),
            transactions,
        };

        let serialized_entry = Bytes::copy_from_slice(&bincode::serialize(&entry).unwrap());
        (entry, serialized_entry)
    }

    #[test]
    fn test_incomplete_header() {
        let mut bytes = Bytes::copy_from_slice(&[0; 47]);
        let mut transactions = Vec::new();
        let handle_transaction = |tx| {
            transactions.push(tx);
        };

        let result = read_entry(&mut bytes, handle_transaction);
        assert!(result.is_err());
    }

    #[test]
    fn test_tick_entry() {
        let (entry, mut bytes) = create_entry_and_serialize(vec![]);
        let mut transactions = Vec::new();
        let handle_transaction = |tx| {
            transactions.push(tx);
        };
        let (num_hashes, hash, num_transactions) =
            read_entry(&mut bytes, handle_transaction).unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
        assert_eq!(num_transactions, entry.transactions.len() as u64);
        assert_eq!(transactions.len(), entry.transactions.len());
    }

    #[test]
    fn test_single_transaction() {
        let (entry, mut bytes) = create_entry_and_serialize(vec![simple_transfer()]);
        let mut transactions = Vec::new();
        let handle_transaction = |tx| {
            transactions.push(tx);
        };
        let (num_hashes, hash, num_transactions) =
            read_entry(&mut bytes, handle_transaction).unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
        assert_eq!(num_transactions, entry.transactions.len() as u64);
        assert_eq!(transactions.len(), entry.transactions.len());

        let transaction = &transactions[0];
        assert_eq!(transaction.signatures().len(), 1);
        assert_eq!(
            transaction
                .instructions_iter()
                .next()
                .unwrap()
                .program_id_index,
            2
        );
    }

    #[test]
    fn test_multiple_transactions() {
        let (entry, mut bytes) = create_entry_and_serialize(vec![
            simple_transfer(),
            simple_transfer(),
            simple_transfer(),
        ]);
        let mut transactions = Vec::new();
        let handle_transaction = |tx| {
            transactions.push(tx);
        };
        let (num_hashes, hash, num_transactions) =
            read_entry(&mut bytes, handle_transaction).unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
        assert_eq!(num_transactions, entry.transactions.len() as u64);
        assert_eq!(transactions.len(), entry.transactions.len());

        for transaction in transactions.iter() {
            assert_eq!(transaction.signatures().len(), 1);
            assert_eq!(
                transaction
                    .instructions_iter()
                    .next()
                    .unwrap()
                    .program_id_index,
                2
            );
        }
    }

    #[test]
    fn test_trailing_bytes() {
        let (entry, bytes) = create_entry_and_serialize(vec![simple_transfer()]);
        let mut bytes = bytes.try_into_mut().unwrap();
        bytes.put_u8(1); // trailing bytes are okay for entries
        let mut bytes = bytes.freeze();

        let mut transactions = Vec::new();
        let handle_transaction = |tx| {
            transactions.push(tx);
        };
        let (num_hashes, hash, num_transactions) =
            read_entry(&mut bytes, handle_transaction).unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
        assert_eq!(num_transactions, entry.transactions.len() as u64);
        assert_eq!(transactions.len(), entry.transactions.len());
    }

    #[test]
    fn test_missing_transaction_byte() {
        let (_entry, bytes) = create_entry_and_serialize(vec![simple_transfer()]);
        let mut bytes = bytes.try_into_mut().unwrap();
        bytes.truncate(bytes.len() - 1); // remove last byte of last transaction
        let mut bytes = bytes.freeze();

        let mut transactions = Vec::new();
        let handle_transaction = |tx| {
            transactions.push(tx);
        };
        assert!(read_entry(&mut bytes, handle_transaction).is_err());
    }

    #[test]
    fn test_missing_transaction() {
        let (_entry, bytes) = create_entry_and_serialize(vec![simple_transfer()]);
        let mut bytes = bytes.try_into_mut().unwrap();
        // modify `num_transactions` to expect 2 transactions
        bytes[40..48].copy_from_slice(2u64.to_le_bytes().as_ref());
        let mut bytes = bytes.freeze();

        let mut transactions = Vec::new();
        let handle_transaction = |tx| {
            transactions.push(tx);
        };
        assert!(read_entry(&mut bytes, handle_transaction).is_err());
    }

    #[test]
    fn test_multiple_entries() {
        let entries = vec![
            Entry {
                num_hashes: 1,
                hash: Hash::new_unique(),
                transactions: vec![simple_transfer()],
            },
            Entry {
                num_hashes: 2,
                hash: Hash::new_unique(),
                transactions: vec![],
            },
            Entry {
                num_hashes: 3,
                hash: Hash::new_unique(),
                transactions: vec![simple_transfer(), simple_transfer()],
            },
        ];
        let mut bytes = Bytes::copy_from_slice(&bincode::serialize(&entries).unwrap());

        // Test type for holding an entry.
        struct TestEntry {
            num_hashes: u64,
            hash: Hash,
            transactions: Vec<UnsanitizedTransactionView<Bytes>>,
        }

        // Read the number of entries from the bytes.
        let num_entries = bincode::deserialize::<u64>(&bytes).unwrap();
        Buf::advance(&mut bytes, core::mem::size_of::<u64>());

        // Read each entry from the bytes.
        let mut entries = Vec::new();
        for _ in 0..num_entries {
            let mut transactions = Vec::new();
            let handle_transaction = |tx| {
                transactions.push(tx);
            };
            let (num_hashes, hash, num_transactions) =
                read_entry(&mut bytes, handle_transaction).unwrap();
            assert_eq!(num_transactions, transactions.len() as u64);
            entries.push(TestEntry {
                num_hashes,
                hash,
                transactions,
            });
        }

        // Verify the entries.
        for (entry, test_entry) in entries.iter().zip(entries.iter()) {
            assert_eq!(entry.num_hashes, test_entry.num_hashes);
            assert_eq!(entry.hash, test_entry.hash);
            assert_eq!(entry.transactions.len(), test_entry.transactions.len());
        }
    }
}
