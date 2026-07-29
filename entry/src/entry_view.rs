//! Replay-side view of a PoH entry.
//!
//! [`EntryView`] is the replay counterpart of [`Entry`]: the same
//! `num_hashes`/`hash` header, but transactions held as zero-copy
//! [`UnsanitizedTransactionView`]s — ref-counted slices of the deshredded
//! payload — instead of deserialized [`VersionedTransaction`]s.
//!
//! [`Entry`] remains the transport type: block production, broadcast, and
//! every other consumer keep serializing and deserializing it through wincode
//! exactly as before. `EntryView` is never serialized; it is only parsed from
//! ledger bytes here, so the transport wire format cannot be affected by the
//! replay migration.
//!
//! [`VersionedTransaction`]: solana_transaction::versioned::VersionedTransaction

use {
    crate::{block_component::VersionedBlockMarker, entry::Entry},
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::UnsanitizedTransactionView,
    },
    bytes::Bytes,
    solana_hash::{HASH_BYTES, Hash},
    thiserror::Error,
};

/// The replay-side counterpart of [`Entry`], parsed zero-copy from serialized
/// entry bytes.
#[derive(Clone, Debug)]
pub struct EntryView {
    /// The number of hashes since the previous Entry ID.
    pub num_hashes: u64,

    /// The SHA-256 hash `num_hashes` after the previous Entry ID.
    pub hash: Hash,

    /// The entry's transactions, each a view over its exact serialized bytes.
    pub transactions: Vec<UnsanitizedTransactionView<Bytes>>,
}

impl EntryView {
    pub fn is_tick(&self) -> bool {
        self.transactions.is_empty()
    }
}

// Manual impl: transaction views are equal iff their serialized transactions
// are equal. The derived impl would also compare frame internals and any
// trailing bytes in the shared backing buffer.
impl PartialEq for EntryView {
    fn eq(&self, other: &Self) -> bool {
        self.num_hashes == other.num_hashes
            && self.hash == other.hash
            && self.transactions.len() == other.transactions.len()
            && self
                .transactions
                .iter()
                .zip(other.transactions.iter())
                .all(|(a, b)| a.data() == b.data())
    }
}

impl Eq for EntryView {}

/// Bridges a transport [`Entry`] into a replay [`EntryView`] by reserializing
/// its transactions. Intended for tests and for code paths that construct
/// entries in memory rather than reading them from the ledger.
impl From<&Entry> for EntryView {
    fn from(entry: &Entry) -> Self {
        Self {
            num_hashes: entry.num_hashes,
            hash: entry.hash,
            transactions: entry
                .transactions
                .iter()
                .map(|transaction| {
                    let bytes = Bytes::from(
                        wincode::serialize(transaction)
                            .expect("VersionedTransaction must be serializable"),
                    );
                    UnsanitizedTransactionView::try_new_unsanitized(bytes)
                        .expect("serialized VersionedTransaction must parse as a transaction view")
                })
                .collect(),
        }
    }
}

#[derive(Debug, Error)]
pub enum EntryViewParseError {
    #[error("unexpected end of data")]
    UnexpectedEnd,
    // Not #[from]: TransactionViewError does not implement std::error::Error.
    #[error("failed to parse transaction: {0:?}")]
    Transaction(TransactionViewError),
    #[error("failed to parse block marker: {0}")]
    BlockMarker(#[from] wincode::ReadError),
}

impl From<TransactionViewError> for EntryViewParseError {
    fn from(err: TransactionViewError) -> Self {
        Self::Transaction(err)
    }
}

/// Parses a serialized `Vec<Entry>` from `bytes` into [`EntryView`]s whose
/// transactions are ref-counted slices of `bytes`.
///
/// Trailing bytes after the entries are ignored, matching
/// `wincode::deserialize::<Vec<Entry>>`. Use [`entry_views_from_bytes_prefix`]
/// to learn how many bytes the entries occupy.
pub fn entry_views_from_bytes(bytes: &Bytes) -> Result<Vec<EntryView>, EntryViewParseError> {
    entry_views_from_bytes_prefix(bytes).map(|(entry_views, _consumed_len)| entry_views)
}

/// Parses a serialized `Vec<Entry>` from the beginning of `bytes`, allowing
/// trailing data.
///
/// Returns the entry views and the number of bytes they occupy, i.e. the
/// offset at which the next item in the buffer begins.
pub fn entry_views_from_bytes_prefix(
    bytes: &Bytes,
) -> Result<(Vec<EntryView>, usize), EntryViewParseError> {
    let mut offset = 0;
    let num_entries = read_u64(bytes, &mut offset)?;
    let mut entry_views =
        Vec::with_capacity(capped_capacity::<EntryView>(num_entries, bytes.len()));
    for _ in 0..num_entries {
        entry_views.push(read_entry_view(bytes, &mut offset)?);
    }
    Ok((entry_views, offset))
}

/// The replay-side counterpart of [`crate::block_component::BlockComponent`]:
/// entry batches are parsed into [`EntryView`]s, while block markers keep the
/// transport type — they contain no transactions to view.
#[derive(Debug, PartialEq, Eq)]
pub enum BlockComponentView {
    EntryBatch(Vec<EntryView>),
    BlockMarker(VersionedBlockMarker),
}

/// Parses a serialized `BlockComponent` payload for replay, mirroring the
/// wincode `BlockComponent` decode: a leading entry count of zero means the
/// payload is a block marker, and a malformed marker fails the payload.
pub fn block_component_view_from_bytes(
    payload: &Bytes,
) -> Result<BlockComponentView, EntryViewParseError> {
    let (entry_views, consumed_len) = entry_views_from_bytes_prefix(payload)?;
    if entry_views.is_empty() {
        let marker = wincode::deserialize(&payload[consumed_len..])?;
        Ok(BlockComponentView::BlockMarker(marker))
    } else {
        Ok(BlockComponentView::EntryBatch(entry_views))
    }
}

fn read_entry_view(bytes: &Bytes, offset: &mut usize) -> Result<EntryView, EntryViewParseError> {
    let num_hashes = read_u64(bytes, offset)?;
    let hash = Hash::new_from_array(read_array::<HASH_BYTES>(bytes, offset)?);
    let num_transactions = read_u64(bytes, offset)?;
    let mut transactions = Vec::with_capacity(
        capped_capacity::<UnsanitizedTransactionView<Bytes>>(num_transactions, bytes.len()),
    );
    for _ in 0..num_transactions {
        transactions.push(read_transaction_view(bytes, offset)?);
    }
    Ok(EntryView {
        num_hashes,
        hash,
        transactions,
    })
}

fn read_transaction_view(
    bytes: &Bytes,
    offset: &mut usize,
) -> Result<UnsanitizedTransactionView<Bytes>, EntryViewParseError> {
    if *offset > bytes.len() {
        return Err(EntryViewParseError::UnexpectedEnd);
    }
    // `slice` only bumps a refcount; the view's accessors are bounded by the
    // parsed transaction length, not by the buffer tail it holds.
    let (view, consumed_len) =
        UnsanitizedTransactionView::try_new_unsanitized_from_prefix(bytes.slice(*offset..))?;
    *offset += consumed_len;
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

fn read_u64(bytes: &Bytes, offset: &mut usize) -> Result<u64, EntryViewParseError> {
    Ok(u64::from_le_bytes(read_array::<8>(bytes, offset)?))
}

fn read_array<const N: usize>(
    bytes: &Bytes,
    offset: &mut usize,
) -> Result<[u8; N], EntryViewParseError> {
    let end = offset
        .checked_add(N)
        .ok_or(EntryViewParseError::UnexpectedEnd)?;
    let array = bytes
        .get(*offset..end)
        .ok_or(EntryViewParseError::UnexpectedEnd)?
        .try_into()
        .expect("slice of length N");
    *offset = end;
    Ok(array)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            block_component::{BlockComponent, BlockHeaderV1},
            entry::next_entry_mut,
        },
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
        // Serialized by the transport type, parsed by the replay type.
        let entries = test_entries();
        let serialized = Bytes::from(wincode::serialize(&entries).unwrap());
        let parsed = entry_views_from_bytes(&serialized).unwrap();
        let expected = entries.iter().map(EntryView::from).collect::<Vec<_>>();
        assert_eq!(expected, parsed);
    }

    #[test]
    fn test_parsed_transactions_deserialize_identically() {
        // Each view's data must be the exact serialized transaction, so the
        // bridge back to VersionedTransaction reproduces the original.
        let entries = test_entries();
        let serialized = Bytes::from(wincode::serialize(&entries).unwrap());
        let parsed = entry_views_from_bytes(&serialized).unwrap();
        for (entry, entry_view) in entries.iter().zip(parsed.iter()) {
            assert_eq!(entry.transactions.len(), entry_view.transactions.len());
            for (transaction, view) in entry.transactions.iter().zip(&entry_view.transactions) {
                assert_eq!(transaction, &wincode::deserialize(view.data()).unwrap());
            }
        }
    }

    #[test]
    fn test_trailing_bytes_after_entries_are_ignored() {
        let entries = test_entries();
        let mut serialized = wincode::serialize(&entries).unwrap();
        let consumed_len = serialized.len();
        serialized.extend_from_slice(&[0xAA; 7]);
        let serialized = Bytes::from(serialized);

        let (parsed, parsed_len) = entry_views_from_bytes_prefix(&serialized).unwrap();
        assert_eq!(
            entries.iter().map(EntryView::from).collect::<Vec<_>>(),
            parsed
        );
        assert_eq!(consumed_len, parsed_len);
    }

    #[test]
    fn test_truncated_input_fails() {
        let entries = test_entries();
        let serialized = wincode::serialize(&entries).unwrap();
        for len in 0..serialized.len() {
            let truncated = Bytes::copy_from_slice(&serialized[..len]);
            assert!(
                entry_views_from_bytes(&truncated).is_err(),
                "truncation to {len} bytes must fail"
            );
        }
    }

    #[test]
    fn test_huge_counts_do_not_preallocate() {
        // A bogus entry count must fail on buffer exhaustion, not abort by
        // preallocating.
        let bytes = Bytes::from(u64::MAX.to_le_bytes().to_vec());
        assert!(matches!(
            entry_views_from_bytes(&bytes),
            Err(EntryViewParseError::UnexpectedEnd)
        ));
    }

    #[test]
    fn test_component_bytes_entry_batch() {
        let entries = test_entries();
        let component = BlockComponent::new_entry_batch(entries.clone()).unwrap();
        let payload = Bytes::from(wincode::serialize(&component).unwrap());
        let parsed = block_component_view_from_bytes(&payload).unwrap();
        assert_eq!(
            BlockComponentView::EntryBatch(entries.iter().map(EntryView::from).collect()),
            parsed
        );
    }

    #[test]
    fn test_component_bytes_block_marker() {
        let component = BlockComponent::new_block_marker(
            crate::block_component::VersionedBlockMarker::from_block_header(BlockHeaderV1 {
                parent_slot: 42,
                parent_block_id: Hash::new_unique(),
            }),
        );
        let payload = Bytes::from(wincode::serialize(&component).unwrap());
        let BlockComponent::BlockMarker(marker) = component else {
            panic!("expected a block marker component");
        };
        assert_eq!(
            block_component_view_from_bytes(&payload).unwrap(),
            BlockComponentView::BlockMarker(marker)
        );

        // A malformed marker must fail the payload, like the wincode
        // BlockComponent decode does.
        let truncated = payload.slice(..payload.len() - 1);
        assert!(block_component_view_from_bytes(&truncated).is_err());

        // An empty entry batch (aborted block) is not a valid marker either.
        let empty_batch = Bytes::from(0u64.to_le_bytes().to_vec());
        assert!(block_component_view_from_bytes(&empty_batch).is_err());
    }
}
