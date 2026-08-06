//! Zero-copy parsing of [`BlockComponentView`] from raw bytes.
//!
//! The wire format is identical to the wincode-derived [`BlockComponent`]
//! (see the module-level docs on [`crate::block_component`] for the full
//! byte layout). Unlike [`BlockComponent`]'s derived [`wincode::SchemaRead`]
//! impl, [`parse`] avoids deserializing transaction bytes into owned
//! structures: each transaction is exposed as an
//! [`UnsanitizedTransactionView`] backed by a zero-copy [`Bytes`] slice of
//! the original buffer.
use {
    crate::{
        block_component::{BlockComponent, BlockComponentView, VersionedBlockMarker},
        entry::EntryView,
    },
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::UnsanitizedTransactionView,
    },
    bytes::Bytes,
    solana_hash::Hash,
    wincode::{SchemaRead, config::DefaultConfig, io::Reader},
};

/// Parses a [`BlockComponentView`] from `bytes`, using the same wire format
/// as [`BlockComponent`].
pub fn parse(bytes: Bytes) -> Result<BlockComponentView, ParseError> {
    let mut header: &[u8] = bytes.as_ref();
    let entry_count =
        <u64 as SchemaRead<DefaultConfig>>::get(header.by_ref()).map_err(ParseError::EntryCount)?;

    if entry_count == 0 {
        let remaining = &bytes[BlockComponent::ENTRY_COUNT_SIZE..];
        let marker = wincode::deserialize_exact::<VersionedBlockMarker>(remaining)
            .map_err(ParseError::BlockMarker)?;
        return Ok(BlockComponentView::BlockMarker(marker));
    }

    let entry_count = usize::try_from(entry_count)
        .map_err(|_| ParseError::EntryCountOverflow { count: entry_count })?;
    if entry_count >= BlockComponent::MAX_ENTRIES {
        return Err(ParseError::TooManyEntries {
            count: entry_count,
            max: BlockComponent::MAX_ENTRIES,
        });
    }

    // Don't pre-allocate, we can't trust the entry_count field to not be malicious
    // causing an OOM.
    let mut entry_views = Vec::new();
    let mut offset = BlockComponent::ENTRY_COUNT_SIZE;

    for entry_index in 0..entry_count {
        let mut cursor: &[u8] = &bytes[offset..];

        let num_hashes =
            <u64 as SchemaRead<DefaultConfig>>::get(cursor.by_ref()).map_err(|source| {
                ParseError::EntryHeader {
                    entry_index,
                    field: "num_hashes",
                    source,
                }
            })?;
        let hash = <Hash as SchemaRead<DefaultConfig>>::get(cursor.by_ref()).map_err(|source| {
            ParseError::EntryHeader {
                entry_index,
                field: "hash",
                source,
            }
        })?;
        let tx_count =
            <u64 as SchemaRead<DefaultConfig>>::get(cursor.by_ref()).map_err(|source| {
                ParseError::EntryHeader {
                    entry_index,
                    field: "tx_count",
                    source,
                }
            })?;

        offset = bytes.len() - cursor.len();

        let tx_count =
            usize::try_from(tx_count).map_err(|_| ParseError::TransactionCountOverflow {
                entry_index,
                count: tx_count,
            })?;

        // Don't pre-allocate, we can't trust the transaction count field to not be malicious
        // causing an OOM.
        let mut transactions = Vec::new();

        for tx_index in 0..tx_count {
            let remaining_bytes = bytes.slice(offset..);
            let (view, consumed_len) =
                UnsanitizedTransactionView::try_new_unsanitized_from_prefix(remaining_bytes)
                    .map_err(|error| ParseError::Transaction {
                        entry_index,
                        tx_index,
                        error,
                    })?;
            offset += consumed_len;
            transactions.push(view);
        }

        entry_views.push(EntryView {
            num_hashes,
            hash,
            transactions,
        });
    }

    if offset != bytes.len() {
        return Err(ParseError::TrailingBytes {
            remaining: bytes.len() - offset,
        });
    }

    Ok(BlockComponentView::EntryBatch(entry_views))
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("failed to read entry count: {0}")]
    EntryCount(wincode::ReadError),

    #[error("entry count {count} does not fit in usize")]
    EntryCountOverflow { count: u64 },

    #[error("entry count {count} exceeds max {max}")]
    TooManyEntries { count: usize, max: usize },

    #[error("failed to read {field} for entry {entry_index}: {source}")]
    EntryHeader {
        entry_index: usize,
        field: &'static str,
        source: wincode::ReadError,
    },

    #[error("transaction count {count} for entry {entry_index} does not fit in usize")]
    TransactionCountOverflow { entry_index: usize, count: u64 },

    #[error("failed to parse transaction {tx_index} of entry {entry_index}: {error:?}")]
    Transaction {
        entry_index: usize,
        tx_index: usize,
        error: TransactionViewError,
    },

    #[error("failed to deserialize block marker: {0}")]
    BlockMarker(wincode::ReadError),

    #[error("{remaining} trailing byte(s) remain after parsing a complete entry batch")]
    TrailingBytes { remaining: usize },
}
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            block_component::{
                BlockFooterV1, BlockHeaderV1, GenesisCertBlockMarker, UpdateParentV1,
            },
            entry::Entry,
        },
        solana_bls_signatures::Keypair as BlsKeypair,
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_transaction::versioned::VersionedTransaction,
        std::iter::repeat_n,
        test_case::test_case,
    };

    fn tick_entries(n: usize) -> Vec<Entry> {
        repeat_n(Entry::default(), n).collect()
    }

    fn entry_with_transactions(num_hashes: u64, transactions: Vec<VersionedTransaction>) -> Entry {
        Entry {
            num_hashes,
            hash: Hash::new_unique(),
            transactions,
        }
    }

    fn transfer_transaction(amount: u64) -> VersionedTransaction {
        solana_system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            amount,
            Hash::default(),
        )
        .into()
    }

    fn sample_footer() -> BlockFooterV1 {
        BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 1234567890,
            block_user_agent: b"test-agent".to_vec(),
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        }
    }

    /// Serializes `entries` as a `BlockComponent` entry batch and parses it back.
    fn parse_entry_batch(entries: Vec<Entry>) -> Vec<EntryView<Bytes>> {
        let component = BlockComponent::new_entry_batch(entries).unwrap();
        let bytes = Bytes::from(wincode::serialize(&component).unwrap());
        let BlockComponentView::EntryBatch(views) = parse(bytes).unwrap() else {
            panic!("expected EntryBatch");
        };
        views
    }

    #[test]
    fn parsed_tick_entry_view_has_same_num_hashes_and_hash_as_source_entry() {
        // Given
        let source_tick_entry = Entry {
            num_hashes: 7,
            hash: Hash::new_unique(),
            transactions: vec![],
        };

        // When
        let parsed_entry_views = parse_entry_batch(vec![source_tick_entry.clone()]);

        // Then
        assert_eq!(parsed_entry_views.len(), 1);
        assert_eq!(
            parsed_entry_views[0].num_hashes,
            source_tick_entry.num_hashes
        );
        assert_eq!(parsed_entry_views[0].hash, source_tick_entry.hash);
        assert!(parsed_entry_views[0].transactions.is_empty());
    }

    #[test]
    fn parsed_entry_views_preserve_source_entry_order_across_multiple_entries() {
        // Given
        let source_entries_with_distinct_num_hashes: Vec<Entry> = (0..5)
            .map(|num_hashes| Entry {
                num_hashes,
                hash: Hash::new_unique(),
                transactions: vec![],
            })
            .collect();

        // When
        let parsed_entry_views = parse_entry_batch(source_entries_with_distinct_num_hashes.clone());

        // Then
        assert_eq!(
            parsed_entry_views.len(),
            source_entries_with_distinct_num_hashes.len()
        );
        for (parsed_entry_view, source_entry) in parsed_entry_views
            .iter()
            .zip(source_entries_with_distinct_num_hashes.iter())
        {
            assert_eq!(parsed_entry_view.num_hashes, source_entry.num_hashes);
            assert_eq!(parsed_entry_view.hash, source_entry.hash);
        }
    }

    #[test]
    fn parsed_transaction_view_exposes_same_signature_as_source_transaction() {
        // Given
        let source_transaction = transfer_transaction(1);
        let source_entry = entry_with_transactions(1, vec![source_transaction.clone()]);

        // When
        let parsed_entry_views = parse_entry_batch(vec![source_entry]);

        // Then
        assert_eq!(parsed_entry_views.len(), 1);
        assert_eq!(parsed_entry_views[0].transactions.len(), 1);
        assert_eq!(
            parsed_entry_views[0].transactions[0].signatures(),
            source_transaction.signatures.as_slice()
        );
    }

    #[test]
    fn parsed_transaction_views_preserve_source_transaction_order_within_entry() {
        // Given
        let first_source_transaction = transfer_transaction(1);
        let second_source_transaction = transfer_transaction(2);
        let source_entry = entry_with_transactions(
            1,
            vec![
                first_source_transaction.clone(),
                second_source_transaction.clone(),
            ],
        );

        // When
        let parsed_entry_views = parse_entry_batch(vec![source_entry]);

        // Then
        let parsed_transaction_views = &parsed_entry_views[0].transactions;
        assert_eq!(parsed_transaction_views.len(), 2);
        assert_eq!(
            parsed_transaction_views[0].signatures(),
            first_source_transaction.signatures.as_slice()
        );
        assert_eq!(
            parsed_transaction_views[1].signatures(),
            second_source_transaction.signatures.as_slice()
        );
    }

    #[test_case(VersionedBlockMarker::from_block_footer(sample_footer()); "block_footer")]
    #[test_case(
        VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 42,
            parent_block_id: Hash::new_unique(),
        });
        "block_header"
    )]
    #[test_case(
        VersionedBlockMarker::from_update_parent(UpdateParentV1 {
            new_parent_slot: 43,
            new_parent_block_id: Hash::new_unique(),
        });
        "update_parent"
    )]
    #[test_case(
        VersionedBlockMarker::from_genesis_cert_block_marker(GenesisCertBlockMarker {
            slot: 44,
            block_id: Hash::new_unique(),
            bls_signature: BlsKeypair::new().sign(b"genesis").into(),
            bitmap: vec![1, 2, 3],
        });
        "genesis_certificate"
    )]
    fn parsed_block_marker_view_matches_source_marker_for_each_variant(
        source_marker: VersionedBlockMarker,
    ) {
        // Given
        let component = BlockComponent::new_block_marker(source_marker.clone());
        let bytes = Bytes::from(wincode::serialize(&component).unwrap());

        // When
        let view = parse(bytes).unwrap();

        // Then
        let BlockComponentView::BlockMarker(parsed_marker) = view else {
            panic!("expected BlockMarker");
        };
        assert_eq!(parsed_marker, source_marker);
    }

    #[test]
    fn parse_rejects_all_zero_empty_entry_batch_payload() {
        // Given
        let empty_entry_batch_payload = Bytes::from(BlockComponent::EMPTY_ENTRY_BATCH.to_vec());

        // When
        let parse_result = parse(empty_entry_batch_payload);

        // Then
        assert!(parse_result.is_err());
    }

    #[test]
    fn parse_rejects_entry_count_header_claiming_max_entries_with_no_further_bytes() {
        // Given
        let mut header_claiming_max_entries =
            (BlockComponent::MAX_ENTRIES as u64).to_le_bytes().to_vec();
        header_claiming_max_entries.resize(BlockComponent::ENTRY_COUNT_SIZE, 0);

        // When
        let parse_result = parse(Bytes::from(header_claiming_max_entries));

        // Then
        assert!(matches!(
            parse_result,
            Err(ParseError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn parse_rejects_extra_trailing_byte_appended_after_valid_entry_batch() {
        // Given
        let component = BlockComponent::new_entry_batch(tick_entries(1)).unwrap();
        let mut entry_batch_bytes_with_trailing_byte = wincode::serialize(&component).unwrap();
        entry_batch_bytes_with_trailing_byte.push(0);

        // When
        let parse_result = parse(Bytes::from(entry_batch_bytes_with_trailing_byte));

        // Then
        assert!(matches!(
            parse_result,
            Err(ParseError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn parse_rejects_extra_trailing_byte_appended_after_valid_block_marker() {
        // Given
        let marker = VersionedBlockMarker::from_block_footer(sample_footer());
        let component = BlockComponent::new_block_marker(marker);
        let mut block_marker_bytes_with_trailing_byte = wincode::serialize(&component).unwrap();
        block_marker_bytes_with_trailing_byte.push(0);

        // When
        let parse_result = parse(Bytes::from(block_marker_bytes_with_trailing_byte));

        // Then
        assert!(matches!(
            parse_result,
            Err(ParseError::BlockMarker(wincode::ReadError::TrailingBytes))
        ));
    }
}
