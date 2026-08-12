use {
    crate::{
        block_component::{BlockComponent, ParsedBlockComponent, VersionedBlockMarker},
        entry::{Entry, EntryView, MAX_DATA_SHREDS_SIZE},
    },
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::UnsanitizedTransactionView,
    },
    bytes::Bytes,
    solana_hash::Hash,
    solana_transaction::versioned::VersionedTransaction,
    std::mem::size_of,
    wincode::{SchemaRead, config::DefaultConfig, io::Reader},
};

/// Parses a [`ParsedBlockComponent`] from `bytes`, using the same wire format
/// as [`BlockComponent`].
pub fn parse<B: Into<Bytes>>(bytes: B) -> Result<ParsedBlockComponent, ParseError> {
    let bytes: Bytes = bytes.into();

    let mut header: &[u8] = bytes.as_ref();
    let entry_count =
        <u64 as SchemaRead<DefaultConfig>>::get(header.by_ref()).map_err(ParseError::EntryCount)?;

    // entry count of zero encodes a block marker
    let entry_count_encodes_block_marker = entry_count == 0;
    if entry_count_encodes_block_marker {
        let remaining = &bytes[BlockComponent::ENTRY_COUNT_SIZE..];
        let marker = wincode::deserialize::<VersionedBlockMarker>(remaining)
            .map_err(ParseError::BlockMarker)?;
        return Ok(ParsedBlockComponent::BlockMarker(marker));
    }

    let entry_count = usize::try_from(entry_count)
        .map_err(|_| ParseError::EntryCountOverflow { count: entry_count })?;
    if entry_count >= BlockComponent::MAX_ENTRIES {
        return Err(ParseError::TooManyEntries {
            count: entry_count,
            max: BlockComponent::MAX_ENTRIES,
        });
    }

    // Since the bytes are wincode serialization of [`BlockComponent::EntryBatch`] which
    // contains regular [`Entry`]s, bound by Entry's size to match wincode's prealloc check
    // behavior for deserialization.
    check_prealloc_limit::<Entry>(entry_count).map_err(|needed| {
        ParseError::EntryCountPreallocLimit {
            count: entry_count,
            needed,
            limit: MAX_DATA_SHREDS_SIZE,
        }
    })?;

    // Safe to pre-allocate: entry_count already passed the preallocation-size check above.
    let mut entry_views = Vec::with_capacity(entry_count);
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
        // Since the bytes are wincode serialization of [`BlockComponent::EntryBatch`] whose
        // entries contain regular [`VersionedTransaction`]s, bound by VersionedTransaction's
        // size to match wincode's prealloc check behavior for deserialization.
        check_prealloc_limit::<VersionedTransaction>(tx_count).map_err(|needed| {
            ParseError::TransactionCountPreallocLimit {
                entry_index,
                count: tx_count,
                needed,
                limit: MAX_DATA_SHREDS_SIZE,
            }
        })?;

        // Safe to pre-allocate: tx_count already passed the preallocation-size check above.
        let mut transactions = Vec::with_capacity(tx_count);

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

    // Trailing bytes after the last entry are intentionally ignored, matching
    // legacy `wincode::deserialize` behavior.
    Ok(ParsedBlockComponent::EntryBatch(entry_views))
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("failed to read entry count: {0}")]
    EntryCount(wincode::ReadError),

    #[error("entry count {count} does not fit in usize")]
    EntryCountOverflow { count: u64 },

    #[error("entry count {count} exceeds max {max}")]
    TooManyEntries { count: usize, max: usize },

    #[error("entry count {count} would need {needed} bytes, exceeding preallocation limit {limit}")]
    EntryCountPreallocLimit {
        count: usize,
        needed: usize,
        limit: usize,
    },

    #[error("failed to read {field} for entry {entry_index}: {source}")]
    EntryHeader {
        entry_index: usize,
        field: &'static str,
        source: wincode::ReadError,
    },

    #[error("transaction count {count} for entry {entry_index} does not fit in usize")]
    TransactionCountOverflow { entry_index: usize, count: u64 },

    #[error(
        "transaction count {count} for entry {entry_index} would need {needed} bytes, exceeding \
         preallocation limit {limit}"
    )]
    TransactionCountPreallocLimit {
        entry_index: usize,
        count: usize,
        needed: usize,
        limit: usize,
    },

    #[error("failed to parse transaction {tx_index} of entry {entry_index}: {error:?}")]
    Transaction {
        entry_index: usize,
        tx_index: usize,
        error: TransactionViewError,
    },

    #[error("failed to deserialize block marker: {0}")]
    BlockMarker(wincode::ReadError),
}

/// Checks that pre-allocating `count` elements of `T` would stay within
/// [`MAX_DATA_SHREDS_SIZE`] bytes (`count * size_of::<T>()`), returning the bytes needed as
/// the error otherwise.
fn check_prealloc_limit<T>(count: usize) -> Result<(), usize> {
    // max(1) to count zero sized types
    let size_of_t = size_of::<T>().max(1);
    let needed = count.saturating_mul(size_of_t);

    if needed > MAX_DATA_SHREDS_SIZE {
        Err(needed)
    } else {
        Ok(())
    }
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
        std::{assert_matches, iter::repeat_n},
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
        let bytes = wincode::serialize(&component).unwrap();
        let ParsedBlockComponent::EntryBatch(views) = parse(bytes).unwrap() else {
            panic!("expected EntryBatch");
        };
        views
    }

    #[test]
    fn parsed_tick_entry_view_has_same_num_hashes_and_hash_as_source_entry() {
        let source_tick_entry = Entry {
            num_hashes: 7,
            hash: Hash::new_unique(),
            transactions: vec![],
        };

        let parsed_entry_views = parse_entry_batch(vec![source_tick_entry.clone()]);

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
        let source_entries_with_distinct_num_hashes: Vec<Entry> = (0..5)
            .map(|num_hashes| Entry {
                num_hashes,
                hash: Hash::new_unique(),
                transactions: vec![],
            })
            .collect();

        let parsed_entry_views = parse_entry_batch(source_entries_with_distinct_num_hashes.clone());

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
        let source_transaction = transfer_transaction(1);
        let source_entry = entry_with_transactions(1, vec![source_transaction.clone()]);

        let parsed_entry_views = parse_entry_batch(vec![source_entry]);

        assert_eq!(parsed_entry_views.len(), 1);
        assert_eq!(parsed_entry_views[0].transactions.len(), 1);
        assert_eq!(
            parsed_entry_views[0].transactions[0].signatures(),
            source_transaction.signatures.as_slice()
        );
    }

    #[test]
    fn parsed_transaction_views_preserve_source_transaction_order_within_entry() {
        let first_source_transaction = transfer_transaction(1);
        let second_source_transaction = transfer_transaction(2);
        let source_entry = entry_with_transactions(
            1,
            vec![
                first_source_transaction.clone(),
                second_source_transaction.clone(),
            ],
        );

        let parsed_entry_views = parse_entry_batch(vec![source_entry]);

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
        let component = BlockComponent::new_block_marker(source_marker.clone());
        let bytes = wincode::serialize(&component).unwrap();

        let view = parse(bytes).unwrap();

        let ParsedBlockComponent::BlockMarker(parsed_marker) = view else {
            panic!("expected BlockMarker");
        };
        assert_eq!(parsed_marker, source_marker);
    }

    #[test]
    fn parse_rejects_all_zero_empty_entry_batch_payload() {
        let empty_entry_batch_payload = Bytes::from(BlockComponent::EMPTY_ENTRY_BATCH.to_vec());

        let parse_result = parse(empty_entry_batch_payload);

        assert!(parse_result.is_err());
    }

    #[test]
    fn parse_rejects_entry_count_header_claiming_max_entries_with_no_further_bytes() {
        let mut header_claiming_max_entries =
            (BlockComponent::MAX_ENTRIES as u64).to_le_bytes().to_vec();
        header_claiming_max_entries.resize(BlockComponent::ENTRY_COUNT_SIZE, 0);

        let parse_result = parse(header_claiming_max_entries);

        assert_matches!(parse_result, Err(ParseError::TooManyEntries { .. }));
    }

    #[test]
    fn parse_ignores_extra_trailing_byte_appended_after_valid_entry_batch() {
        let source_entries = tick_entries(1);
        let component = BlockComponent::new_entry_batch(source_entries.clone()).unwrap();
        let mut entry_batch_bytes_with_trailing_byte = wincode::serialize(&component).unwrap();
        entry_batch_bytes_with_trailing_byte.push(0);

        let parse_result = parse(entry_batch_bytes_with_trailing_byte);

        // trailing bytes are ignored matching legacy `wincode::deserialize`
        let Ok(ParsedBlockComponent::EntryBatch(views)) = parse_result else {
            panic!("expected EntryBatch");
        };
        assert_eq!(views.len(), source_entries.len());
    }

    #[test]
    fn parse_ignores_extra_trailing_byte_appended_after_valid_block_marker() {
        let marker = VersionedBlockMarker::from_block_footer(sample_footer());
        let component = BlockComponent::new_block_marker(marker.clone());
        let mut block_marker_bytes_with_trailing_byte = wincode::serialize(&component).unwrap();
        block_marker_bytes_with_trailing_byte.push(0);

        let parse_result = parse(block_marker_bytes_with_trailing_byte);

        let Ok(ParsedBlockComponent::BlockMarker(parsed_marker)) = parse_result else {
            panic!("expected BlockMarker");
        };
        assert_eq!(parsed_marker, marker);
    }

    #[test]
    fn parse_rejects_entry_count_over_prealloc_limit() {
        // One past the largest entry_count that fits the preallocation-size guard, but
        // still well under MAX_ENTRIES, so only the prealloc check can reject it.
        let entry_count = MAX_DATA_SHREDS_SIZE / size_of::<Entry>() + 1;
        assert!(entry_count < BlockComponent::MAX_ENTRIES);

        // Just the entry_count header; no entry data needs to follow it.
        let header = (entry_count as u64).to_le_bytes().to_vec();

        let owned_result = wincode::deserialize::<BlockComponent>(&header);
        let view_result = parse(header);

        assert_matches!(
            owned_result,
            Err(wincode::ReadError::PreallocationSizeLimit { .. })
        );
        assert_matches!(view_result, Err(ParseError::EntryCountPreallocLimit { .. }));
    }

    #[test]
    fn parse_accepts_entry_count_at_prealloc_limit() {
        // The largest entry_count that still fits the preallocation-size guard. No
        // entry data follows it, so both decoders still error out - just not because
        // of this guard, which is the only thing this test checks.
        let entry_count = MAX_DATA_SHREDS_SIZE / size_of::<Entry>();
        let header = (entry_count as u64).to_le_bytes().to_vec();

        let owned_result = wincode::deserialize::<BlockComponent>(&header);
        let view_result = parse(header);

        assert!(!matches!(
            owned_result,
            Err(wincode::ReadError::PreallocationSizeLimit { .. })
        ));
        assert!(!matches!(
            view_result,
            Err(ParseError::EntryCountPreallocLimit { .. })
        ));
    }

    #[test]
    fn parse_rejects_transaction_count_over_prealloc_limit() {
        // A real single-entry, zero-transaction batch, with its tx_count field (the
        // last 8 bytes, since no transactions follow it) patched to one past the
        // largest tx_count that fits the preallocation-size guard.
        let component = BlockComponent::new_entry_batch(vec![Entry::default()]).unwrap();
        let mut bytes = wincode::serialize(&component).unwrap();

        let tx_count = MAX_DATA_SHREDS_SIZE / size_of::<VersionedTransaction>() + 1;
        let tx_count_offset = bytes.len() - size_of::<u64>();
        bytes[tx_count_offset..].copy_from_slice(&(tx_count as u64).to_le_bytes());

        let owned_result = wincode::deserialize::<BlockComponent>(&bytes);
        let view_result = parse(bytes);

        assert_matches!(
            owned_result,
            Err(wincode::ReadError::PreallocationSizeLimit { .. })
        );
        assert_matches!(
            view_result,
            Err(ParseError::TransactionCountPreallocLimit { .. })
        );
    }

    #[test]
    fn parse_accepts_transaction_count_at_prealloc_limit() {
        // The largest tx_count that still fits the preallocation-size guard, patched
        // into a real zero-transaction entry. No transaction data follows it, so both
        // decoders still error out - just not because of this guard.
        let component = BlockComponent::new_entry_batch(vec![Entry::default()]).unwrap();
        let mut bytes = wincode::serialize(&component).unwrap();

        let tx_count = MAX_DATA_SHREDS_SIZE / size_of::<VersionedTransaction>();
        let tx_count_offset = bytes.len() - size_of::<u64>();
        bytes[tx_count_offset..].copy_from_slice(&(tx_count as u64).to_le_bytes());

        let owned_result = wincode::deserialize::<BlockComponent>(&bytes);
        let view_result = parse(bytes);

        assert!(!matches!(
            owned_result,
            Err(wincode::ReadError::PreallocationSizeLimit { .. })
        ));
        assert!(!matches!(
            view_result,
            Err(ParseError::TransactionCountPreallocLimit { .. })
        ));
    }

    const ENTRY_PREALLOC_LIMIT: usize = MAX_DATA_SHREDS_SIZE / size_of::<Entry>();

    #[test_case(0; "zero_count")]
    #[test_case(1; "one_count")]
    #[test_case(ENTRY_PREALLOC_LIMIT; "at_limit")]
    fn check_prealloc_limit_for_entry_accepts_within_limit(count: usize) {
        assert_matches!(check_prealloc_limit::<Entry>(count), Ok(()));
    }

    #[test_case(ENTRY_PREALLOC_LIMIT + 1; "over_limit")]
    #[test_case(usize::MAX; "count_overflows_and_saturates")]
    fn check_prealloc_limit_for_entry_rejects_count_over_limit(count: usize) {
        assert_eq!(
            check_prealloc_limit::<Entry>(count),
            Err(count.saturating_mul(size_of::<Entry>()))
        );
    }

    #[test]
    fn check_prealloc_limit_treats_zero_sized_types_as_one_byte_each() {
        struct ZeroSized;
        assert_eq!(size_of::<ZeroSized>(), 0);

        let count = MAX_DATA_SHREDS_SIZE + 1;

        assert_eq!(check_prealloc_limit::<ZeroSized>(count), Err(count));
    }
}
