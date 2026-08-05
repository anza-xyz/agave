//! Parsing of serialized entries into byte-backed transaction views.

use {
    crate::entry::{Entry, EntryView, MAX_DATA_SHREDS_SIZE},
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::UnsanitizedTransactionView,
    },
    bytes::Bytes,
    solana_hash::{HASH_BYTES, Hash},
    solana_transaction::versioned::VersionedTransaction,
    std::mem::size_of,
    thiserror::Error,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EntryParseError {
    #[error("unexpected end of entry data")]
    UnexpectedEnd,
    #[error("entry sequence length does not fit in usize")]
    LengthOverflow,
    #[error(
        "entry sequence preallocation limit exceeded: {count} items of {element_size} bytes \
         exceeds {limit} bytes"
    )]
    PreallocationLimit {
        count: usize,
        element_size: usize,
        limit: usize,
    },
    #[error("failed to parse transaction: {0:?}")]
    Transaction(TransactionViewError),
}

impl From<TransactionViewError> for EntryParseError {
    fn from(error: TransactionViewError) -> Self {
        Self::Transaction(error)
    }
}

/// Parse a serialized `Vec<Entry>`.
///
/// Trailing bytes are accepted, matching `wincode::deserialize`.
pub fn entries_from_bytes(bytes: &Bytes) -> Result<Vec<EntryView>, EntryParseError> {
    entries_from_bytes_prefix(bytes).map(|(entries, _consumed_len)| entries)
}

/// Parse a serialized `Vec<Entry>` prefix and return its consumed length.
pub fn entries_from_bytes_prefix(
    bytes: &Bytes,
) -> Result<(Vec<EntryView>, usize), EntryParseError> {
    let mut offset = 0;
    let num_entries = read_len(bytes, &mut offset)?;
    // Match WincodeVec<Entry, MaxDataShredsLen>'s allocation check exactly. Using EntryView here
    // would accept a different maximum count because its in-memory size is different.
    check_preallocation::<Entry>(num_entries)?;

    let mut entries = Vec::with_capacity(num_entries);
    for _ in 0..num_entries {
        entries.push(read_entry(bytes, &mut offset)?);
    }
    Ok((entries, offset))
}

fn read_entry(bytes: &Bytes, offset: &mut usize) -> Result<EntryView, EntryParseError> {
    let num_hashes = read_u64(bytes, offset)?;
    let hash = Hash::new_from_array(read_array::<HASH_BYTES>(bytes, offset)?);
    let num_transactions = read_len(bytes, offset)?;
    // Preserve the historical WincodeVec<VersionedTransaction, MaxDataShredsLen>
    // allocation check even though the replay representation stores smaller view metadata.
    check_preallocation::<VersionedTransaction>(num_transactions)?;

    let mut transactions = Vec::with_capacity(num_transactions);
    for _ in 0..num_transactions {
        let remaining = bytes.get(*offset..).ok_or(EntryParseError::UnexpectedEnd)?;
        let remaining = bytes.slice_ref(remaining);
        let (transaction, consumed_len) =
            UnsanitizedTransactionView::try_new_unsanitized_from_prefix(remaining)?;
        *offset = offset
            .checked_add(consumed_len)
            .ok_or(EntryParseError::LengthOverflow)?;
        transactions.push(transaction);
    }

    Ok(EntryView {
        num_hashes,
        hash,
        transactions,
    })
}

fn check_preallocation<T>(count: usize) -> Result<(), EntryParseError> {
    let element_size = size_of::<T>().max(1);
    if <crate::entry::MaxDataShredsLen as wincode::len::SeqLen<
        wincode::config::DefaultConfig,
    >>::prealloc_check::<T>(count)
    .is_err()
    {
        return Err(EntryParseError::PreallocationLimit {
            count,
            element_size,
            limit: MAX_DATA_SHREDS_SIZE,
        });
    }
    Ok(())
}

fn read_len(bytes: &Bytes, offset: &mut usize) -> Result<usize, EntryParseError> {
    usize::try_from(read_u64(bytes, offset)?).map_err(|_| EntryParseError::LengthOverflow)
}

fn read_u64(bytes: &Bytes, offset: &mut usize) -> Result<u64, EntryParseError> {
    Ok(u64::from_le_bytes(read_array::<8>(bytes, offset)?))
}

fn read_array<const N: usize>(
    bytes: &Bytes,
    offset: &mut usize,
) -> Result<[u8; N], EntryParseError> {
    let end = offset
        .checked_add(N)
        .ok_or(EntryParseError::UnexpectedEnd)?;
    let array = bytes
        .get(*offset..end)
        .ok_or(EntryParseError::UnexpectedEnd)?
        .try_into()
        .map_err(|_| EntryParseError::UnexpectedEnd)?;
    *offset = end;
    Ok(array)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::entry::{next_entry_mut, versioned_transaction_from_view},
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_system_transaction as system_transaction,
        solana_transaction::Transaction,
    };

    fn test_transaction() -> Transaction {
        let keypair = Keypair::new();
        system_transaction::transfer(&keypair, &Pubkey::new_unique(), 1, Hash::new_unique())
    }

    fn test_entries() -> Vec<Entry> {
        let mut hash = Hash::new_unique();
        vec![
            next_entry_mut(&mut hash, 1, vec![]),
            next_entry_mut(&mut hash, 1, vec![test_transaction(), test_transaction()]),
            next_entry_mut(&mut hash, 1, vec![test_transaction()]),
        ]
    }

    fn assert_views_match_wire(views: &[EntryView], wire: &[Entry]) {
        assert_eq!(views.len(), wire.len());
        for (view, wire) in views.iter().zip(wire) {
            assert_eq!(view.num_hashes, wire.num_hashes);
            assert_eq!(view.hash, wire.hash);
            assert_eq!(
                view.transactions
                    .iter()
                    .map(|transaction| {
                        versioned_transaction_from_view(transaction)
                            .expect("parsed test transaction must decode")
                    })
                    .collect::<Vec<_>>(),
                wire.transactions
            );
        }
    }

    #[derive(wincode::SchemaWrite, wincode::SchemaRead)]
    struct LegacyEntry {
        num_hashes: u64,
        hash: Hash,
        #[wincode(
            with = "wincode::containers::Vec<VersionedTransaction, crate::entry::MaxDataShredsLen>"
        )]
        transactions: Vec<VersionedTransaction>,
    }

    #[test]
    fn test_round_trip() {
        let entries = test_entries();
        let serialized = Bytes::from(wincode::serialize(&entries).unwrap());
        let parsed = entries_from_bytes(&serialized).unwrap();
        assert_views_match_wire(&parsed, &entries);
        assert_eq!(
            wincode::deserialize::<Vec<Entry>>(&serialized).unwrap(),
            entries
        );

        let serialized_range =
            serialized.as_ptr() as usize..serialized.as_ptr() as usize + serialized.len();
        for transaction in parsed.iter().flat_map(|entry| entry.transactions.iter()) {
            let transaction_range = transaction.data().as_ptr() as usize
                ..transaction.data().as_ptr() as usize + transaction.data().len();
            assert!(
                serialized_range.start <= transaction_range.start
                    && transaction_range.end <= serialized_range.end,
                "parsed transaction should borrow the serialized entry allocation",
            );
        }
    }

    #[test]
    fn test_wire_format_matches_legacy_entry() {
        let entries = test_entries();
        let legacy_entries = entries
            .iter()
            .map(|entry| LegacyEntry {
                num_hashes: entry.num_hashes,
                hash: entry.hash,
                transactions: entry.transactions.clone(),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            wincode::serialize(&entries).unwrap(),
            wincode::serialize(&legacy_entries).unwrap()
        );
    }

    #[test]
    fn test_parser_matches_legacy_decoder() {
        let entries = test_entries();
        let bytes = Bytes::from(wincode::serialize(&entries).unwrap());
        let parsed = entries_from_bytes(&bytes).unwrap();
        let legacy = wincode::deserialize::<Vec<LegacyEntry>>(&bytes).unwrap();

        assert_eq!(parsed.len(), legacy.len());
        for (entry, legacy) in parsed.iter().zip(legacy) {
            assert_eq!(entry.num_hashes, legacy.num_hashes);
            assert_eq!(entry.hash, legacy.hash);
            assert_eq!(
                entry
                    .transactions
                    .iter()
                    .map(|transaction| {
                        versioned_transaction_from_view(transaction)
                            .expect("parsed test transaction must decode")
                    })
                    .collect::<Vec<_>>(),
                legacy.transactions
            );
        }
    }

    #[test]
    fn test_trailing_bytes_are_accepted_and_reported() {
        let entries = test_entries();
        let mut serialized = wincode::serialize(&entries).unwrap();
        let consumed_len = serialized.len();
        serialized.extend_from_slice(&[0xaa; 7]);
        let serialized = Bytes::from(serialized);

        let (parsed, parsed_len) = entries_from_bytes_prefix(&serialized).unwrap();
        assert_views_match_wire(&parsed, &entries);
        assert_eq!(parsed_len, consumed_len);
        assert_views_match_wire(&entries_from_bytes(&serialized).unwrap(), &entries);
    }

    #[test]
    fn test_every_truncation_fails() {
        let serialized = wincode::serialize(&test_entries()).unwrap();
        for len in 0..serialized.len() {
            let truncated = Bytes::copy_from_slice(&serialized[..len]);
            assert!(
                entries_from_bytes(&truncated).is_err(),
                "truncation at {len} bytes unexpectedly parsed"
            );
        }
    }

    #[test]
    fn test_huge_count_does_not_allocate() {
        let bytes = Bytes::from(u64::MAX.to_le_bytes().to_vec());
        assert!(matches!(
            entries_from_bytes(&bytes),
            Err(EntryParseError::PreallocationLimit { .. })
        ));
    }

    #[test]
    fn test_wire_entry_accepts_zero_signatures_but_replay_view_rejects_it() {
        let entry = Entry {
            num_hashes: 1,
            hash: Hash::default(),
            transactions: vec![VersionedTransaction::from(Transaction::new_with_payer(
                &[],
                None,
            ))],
        };
        let serialized = Bytes::from(wincode::serialize(&vec![entry.clone()]).unwrap());

        assert_eq!(
            wincode::deserialize::<Vec<Entry>>(&serialized).unwrap(),
            vec![entry]
        );
        assert!(matches!(
            entries_from_bytes(&serialized),
            Err(EntryParseError::Transaction(
                TransactionViewError::ParseError
            ))
        ));
    }
}
