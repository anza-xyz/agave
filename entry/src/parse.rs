//! Byte-level parsing of serialized entries into [`Entry`] values whose
//! transactions are zero-copy views into the source buffer.
//!
//! [`Entry`] serializes through wincode ([`SchemaWrite`]), byte-identically to
//! the historical `Vec<VersionedTransaction>` encoding, but it cannot
//! implement `SchemaRead`: a serialized transaction's length is only
//! discoverable by parsing the transaction itself, and a wincode reader only
//! hands out byte counts that are known up front. Entries are instead parsed
//! directly out of a [`Bytes`] buffer here, so that each transaction view is a
//! cheap, ref-counted slice of the deshredded payload.
//!
//! [`SchemaWrite`]: wincode::SchemaWrite

use {
    crate::entry::Entry,
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::UnsanitizedTransactionView,
    },
    bytes::Bytes,
    solana_hash::{HASH_BYTES, Hash},
    thiserror::Error,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EntryParseError {
    #[error("unexpected end of data")]
    UnexpectedEnd,
    // Not #[from]: TransactionViewError does not implement std::error::Error.
    #[error("failed to parse transaction: {0:?}")]
    Transaction(TransactionViewError),
}

impl From<TransactionViewError> for EntryParseError {
    fn from(err: TransactionViewError) -> Self {
        Self::Transaction(err)
    }
}

/// Parses a serialized `Vec<Entry>` from `bytes`.
///
/// Trailing bytes after the entries are ignored, matching the historical
/// `wincode::deserialize::<Vec<Entry>>` behavior. Use
/// [`entries_from_bytes_prefix`] to learn how many bytes the entries occupy.
pub fn entries_from_bytes(bytes: &Bytes) -> Result<Vec<Entry>, EntryParseError> {
    entries_from_bytes_prefix(bytes).map(|(entries, _consumed_len)| entries)
}

/// Parses a serialized `Vec<Entry>` from the beginning of `bytes`, allowing
/// trailing data.
///
/// Returns the entries and the number of bytes they occupy, i.e. the offset at
/// which the next item in the buffer begins.
pub fn entries_from_bytes_prefix(bytes: &Bytes) -> Result<(Vec<Entry>, usize), EntryParseError> {
    let mut offset = 0;
    let num_entries = read_u64(bytes, &mut offset)?;
    let mut entries = Vec::with_capacity(capped_capacity::<Entry>(num_entries, bytes.len()));
    for _ in 0..num_entries {
        entries.push(read_entry(bytes, &mut offset)?);
    }
    Ok((entries, offset))
}

fn read_entry(bytes: &Bytes, offset: &mut usize) -> Result<Entry, EntryParseError> {
    let num_hashes = read_u64(bytes, offset)?;
    let hash = Hash::new_from_array(read_array::<HASH_BYTES>(bytes, offset)?);
    let num_transactions = read_u64(bytes, offset)?;
    let mut transactions = Vec::with_capacity(
        capped_capacity::<UnsanitizedTransactionView<Bytes>>(num_transactions, bytes.len()),
    );
    for _ in 0..num_transactions {
        transactions.push(read_transaction_view(bytes, offset)?);
    }
    Ok(Entry {
        num_hashes,
        hash,
        transactions,
    })
}

fn read_transaction_view(
    bytes: &Bytes,
    offset: &mut usize,
) -> Result<UnsanitizedTransactionView<Bytes>, EntryParseError> {
    let remaining = bytes.get(*offset..).ok_or(EntryParseError::UnexpectedEnd)?;
    // First parse learns the transaction's length; the view is then rebuilt
    // over an exact-length sub-slice so that its backing data is precisely the
    // serialized transaction, independent of whatever follows it in the
    // buffer. `Bytes::slice` only bumps a refcount.
    let (_, transaction_len) =
        UnsanitizedTransactionView::try_new_unsanitized_from_prefix(remaining)?;
    let end = *offset + transaction_len;
    let view = UnsanitizedTransactionView::try_new_unsanitized(bytes.slice(*offset..end))?;
    *offset = end;
    Ok(view)
}

/// A preallocation bound for a length read from untrusted data: no valid
/// buffer holds more items than bytes remaining.
fn capped_capacity<T>(claimed_len: u64, bytes_len: usize) -> usize {
    usize::try_from(claimed_len)
        .unwrap_or(usize::MAX)
        .min(bytes_len)
        .min(wincode::config::DEFAULT_PREALLOCATION_SIZE_LIMIT / size_of::<T>())
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
        .expect("slice of length N");
    *offset = end;
    Ok(array)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::entry::{Entry, next_entry_mut},
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

    #[test]
    fn test_round_trip() {
        let entries = test_entries();
        let serialized = Bytes::from(wincode::serialize(&entries).unwrap());
        let parsed = entries_from_bytes(&serialized).unwrap();
        assert_eq!(entries, parsed);
    }

    #[test]
    fn test_wire_format_matches_versioned_transaction_encoding() {
        // The serialized form of `Vec<Entry>` must remain byte-identical to
        // the historical `Vec<VersionedTransaction>`-based encoding: it is
        // consensus wire format.
        use solana_transaction::versioned::VersionedTransaction;

        #[derive(wincode::SchemaWrite)]
        struct LegacyEntry {
            num_hashes: u64,
            hash: Hash,
            transactions: Vec<VersionedTransaction>,
        }

        let entries = test_entries();
        let legacy_entries = entries
            .iter()
            .map(|entry| LegacyEntry {
                num_hashes: entry.num_hashes,
                hash: entry.hash,
                transactions: entry
                    .transactions
                    .iter()
                    .map(|tx_view| wincode::deserialize(tx_view.data()).unwrap())
                    .collect(),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            wincode::serialize(&entries).unwrap(),
            wincode::serialize(&legacy_entries).unwrap()
        );
    }

    #[test]
    fn test_trailing_bytes_after_entries_are_ignored() {
        let entries = test_entries();
        let mut serialized = wincode::serialize(&entries).unwrap();
        let consumed_len = serialized.len();
        serialized.extend_from_slice(&[0xAA; 7]);
        let serialized = Bytes::from(serialized);

        let (parsed, parsed_len) = entries_from_bytes_prefix(&serialized).unwrap();
        assert_eq!(entries, parsed);
        assert_eq!(consumed_len, parsed_len);
        assert_eq!(entries, entries_from_bytes(&serialized).unwrap());
    }

    #[test]
    fn test_truncated_input_fails() {
        let entries = test_entries();
        let serialized = wincode::serialize(&entries).unwrap();
        for len in 0..serialized.len() {
            let truncated = Bytes::copy_from_slice(&serialized[..len]);
            assert!(
                entries_from_bytes(&truncated).is_err(),
                "truncation to {len} bytes must fail"
            );
        }
    }

    #[test]
    fn test_huge_counts_do_not_preallocate() {
        // A bogus entry count must fail on buffer exhaustion, not abort by
        // preallocating.
        let bytes = Bytes::from(u64::MAX.to_le_bytes().to_vec());
        assert_eq!(
            entries_from_bytes(&bytes),
            Err(EntryParseError::UnexpectedEnd)
        );
    }
}
