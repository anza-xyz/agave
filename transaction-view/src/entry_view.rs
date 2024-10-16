use {
    crate::{
        bytes::{check_remaining, read_type},
        result::{Result, TransactionViewError},
        transaction_data::TransactionData,
        transaction_frame::TransactionFrame,
        transaction_view::{TransactionView, UnsanitizedTransactionView},
    },
    solana_sdk::hash::Hash,
    std::sync::Arc,
};

pub trait EntryData: Clone {
    fn data(&self) -> &[u8];
}

impl EntryData for Arc<Vec<u8>> {
    #[inline]
    fn data(&self) -> &[u8] {
        self.as_ref()
    }
}

pub struct EntryTransactionData<D: EntryData> {
    data: D,
    offset: usize,
}

impl<D: EntryData> TransactionData for EntryTransactionData<D> {
    #[inline]
    fn data(&self) -> &[u8] {
        &self.data.data()[self.offset..]
    }
}

pub fn read_entry<D: EntryData>(
    data: D,
) -> Result<(
    u64,
    Hash,
    impl ExactSizeIterator<Item = Result<TransactionView<false, EntryTransactionData<D>>>>,
)> {
    let mut offset = 0;
    let (num_hashes, hash, num_transactions) = read_entry_header(data.data(), &mut offset)?;
    let transactions = (0..num_transactions as usize)
        .map(move |_| read_transaction_view(data.clone(), &mut offset));

    Ok((num_hashes, hash, transactions))
}

/// Read the header information from an entry.
/// This includes:
/// - `num_hashes` - u64
/// - `hash` - Hash
/// - `num_transactions` - u64
#[inline]
fn read_entry_header(bytes: &[u8], offset: &mut usize) -> Result<(u64, Hash, u64)> {
    Ok((
        read_u64(bytes, offset)?,
        *unsafe { read_type::<Hash>(bytes, offset)? },
        read_u64(bytes, offset)?,
    ))
}

/// This function reads a `u64` WITHOUT assuming the alignment of the data.
#[inline]
fn read_u64(bytes: &[u8], offset: &mut usize) -> Result<u64> {
    check_remaining(bytes, *offset, core::mem::size_of::<u64>())?;
    let value = u64::from_le_bytes(
        bytes[*offset..*offset + core::mem::size_of::<u64>()]
            .try_into()
            .map_err(|_| TransactionViewError::ParseError)?,
    );
    *offset += core::mem::size_of::<u64>();
    Ok(value)
}

/// This will clone the `data` for each transaction, but this should
/// be relatively cheap.
#[inline]
fn read_transaction_view<D: EntryData>(
    data: D,
    offset: &mut usize,
) -> Result<UnsanitizedTransactionView<EntryTransactionData<D>>> {
    let (frame, bytes_read) = TransactionFrame::try_new(&data.data()[*offset..])?;
    // SAFETY: The format was verified by `TransactionFrame::try_new`.
    let view = unsafe {
        UnsanitizedTransactionView::from_data_unchecked(
            EntryTransactionData {
                data,
                offset: *offset,
            },
            frame,
        )
    };
    *offset += bytes_read;
    Ok(view)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
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

    fn create_entry_and_serialize(
        transactions: Vec<VersionedTransaction>,
    ) -> (Entry, Arc<Vec<u8>>) {
        let entry = Entry {
            num_hashes: 42,
            hash: Hash::default(),
            transactions,
        };

        let serialized_entry = Arc::new(bincode::serialize(&entry).unwrap());
        (entry, serialized_entry)
    }

    #[test]
    fn test_tick_entry() {
        let (entry, bytes) = create_entry_and_serialize(vec![]);
        let (num_hashes, hash, transactions) = read_entry(bytes).unwrap();
        let transactions = transactions.collect::<Result<Vec<_>>>().unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
        assert_eq!(transactions.len(), entry.transactions.len());
    }

    #[test]
    fn test_single_transaction() {
        let (entry, bytes) = create_entry_and_serialize(vec![simple_transfer()]);
        let (num_hashes, hash, transactions) = read_entry(bytes).unwrap();
        let transactions = transactions.collect::<Result<Vec<_>>>().unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
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
        let (entry, bytes) = create_entry_and_serialize(vec![
            simple_transfer(),
            simple_transfer(),
            simple_transfer(),
        ]);
        let (num_hashes, hash, transactions) = read_entry(bytes).unwrap();
        let transactions = transactions.collect::<Result<Vec<_>>>().unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
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
        let (entry, mut bytes) = create_entry_and_serialize(vec![simple_transfer()]);
        Arc::make_mut(&mut bytes).push(0); // trailing bytes are okay for entries.

        let (num_hashes, hash, transactions) = read_entry(bytes).unwrap();
        let transactions = transactions.collect::<Result<Vec<_>>>().unwrap();

        assert_eq!(num_hashes, entry.num_hashes);
        assert_eq!(hash, entry.hash);
        assert_eq!(transactions.len(), entry.transactions.len());
    }
}
