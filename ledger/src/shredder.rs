use {
    crate::shred::{BuildError, Data, ParseError, ProcessShredsStats, Shred, ShredView},
    agave_shred::{
        kind,
        shredder::{BatchPosition, FecSet, FecSetSpec},
    },
    solana_clock::Slot,
    solana_entry::{block_component::BlockComponent, entry::Entry},
    solana_hash::Hash,
    solana_keypair::Keypair,
    std::time::Instant,
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum DeshredError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("shred indices are not consecutive")]
    NonConsecutive,
    #[error("the last shred does not complete the data")]
    Incomplete,
    #[error("shreds follow the data-complete shred")]
    TrailingShreds,
}

#[derive(Debug)]
pub struct Shredder {
    slot: Slot,
    parent_slot: Slot,
    version: u16,
    reference_tick: u8,
}

impl Shredder {
    pub fn new(
        slot: Slot,
        parent_slot: Slot,
        reference_tick: u8,
        version: u16,
    ) -> Result<Self, BuildError> {
        if slot < parent_slot || slot - parent_slot > u64::from(u16::MAX) {
            Err(BuildError::BadParentSlot { slot, parent_slot })
        } else {
            Ok(Self {
                slot,
                parent_slot,
                reference_tick,
                version,
            })
        }
    }

    pub fn make_merkle_shreds_from_component(
        &self,
        keypair: &Keypair,
        component: &BlockComponent,
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        stats: &mut ProcessShredsStats,
    ) -> Vec<Shred> {
        let now = Instant::now();
        let bytes = wincode::serialize(component).unwrap();
        stats.serialize_elapsed += now.elapsed().as_micros() as u64;
        self.make_shreds_from_data_slice(
            keypair,
            &bytes,
            is_last_in_slot,
            chained_merkle_root,
            next_shred_index,
            stats,
        )
        .unwrap()
    }

    pub fn make_merkle_shreds_from_entries(
        &self,
        keypair: &Keypair,
        entries: &[Entry],
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        stats: &mut ProcessShredsStats,
    ) -> Vec<Shred> {
        stats.num_entries += entries.len();
        let now = Instant::now();
        let entries = wincode::serialize(entries).unwrap();
        stats.serialize_elapsed += now.elapsed().as_micros() as u64;
        self.make_shreds_from_data_slice(
            keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            next_shred_index,
            stats,
        )
        .unwrap()
    }

    pub fn make_shreds_from_data_slice(
        &self,
        keypair: &Keypair,
        data: &[u8],
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        stats: &mut ProcessShredsStats,
    ) -> Result<Vec<Shred>, BuildError> {
        let now = Instant::now();
        let final_position = if is_last_in_slot {
            BatchPosition::LastInSlot
        } else {
            BatchPosition::DataComplete
        };
        let mut spec = FecSetSpec {
            slot: self.slot,
            parent_slot: self.parent_slot,
            version: self.version,
            reference_tick: self.reference_tick,
            fec_set_index: next_shred_index,
            chained_merkle_root,
            batch_position: final_position,
        };
        let mut rest = data;
        let mut shreds = Vec::new();
        loop {
            spec.batch_position = final_position;
            let ends_data = rest.len() <= spec.capacity();
            if !ends_data {
                spec.batch_position = BatchPosition::Interior;
            }
            let take = rest.len().min(spec.capacity());
            let (chunk, tail) = rest.split_at(take);
            let batch = FecSet::build(&spec, chunk, keypair)?;
            stats.padding_bytes += spec.capacity() - take;
            spec.chained_merkle_root = batch.merkle_root;
            spec.fec_set_index = spec
                .fec_set_index
                .checked_add(agave_shred::constants::DATA_SHREDS_PER_FEC_BLOCK)
                .ok_or(BuildError::IndexOverflow)?;
            shreds.extend(batch.into_any());
            rest = tail;
            if ends_data {
                break;
            }
        }
        stats.data_bytes += data.len();
        stats.record_num_data_shreds(shreds.len() / 2);
        for shred in &shreds {
            stats.record_shred(shred);
        }
        stats.gen_data_elapsed += now.elapsed().as_micros() as u64;
        Ok(shreds)
    }

    pub fn entries_to_merkle_shreds_for_tests(
        &self,
        keypair: &Keypair,
        entries: &[Entry],
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        stats: &mut ProcessShredsStats,
    ) -> (Vec<Shred>, Vec<Shred>) {
        self.make_merkle_shreds_from_entries(
            keypair,
            entries,
            is_last_in_slot,
            chained_merkle_root,
            next_shred_index,
            stats,
        )
        .into_iter()
        .partition(Shred::is_data)
    }

    pub fn component_to_merkle_shreds_for_tests(
        &self,
        keypair: &Keypair,
        component: &BlockComponent,
        is_last_in_slot: bool,
        chained_merkle_root: Hash,
        next_shred_index: u32,
        stats: &mut ProcessShredsStats,
    ) -> (Vec<Shred>, Vec<Shred>) {
        self.make_merkle_shreds_from_component(
            keypair,
            component,
            is_last_in_slot,
            chained_merkle_root,
            next_shred_index,
            stats,
        )
        .into_iter()
        .partition(Shred::is_data)
    }

    pub fn deshred<I, T: AsRef<[u8]>>(shreds: I) -> Result<Vec<u8>, DeshredError>
    where
        I: IntoIterator<Item = T>,
    {
        let mut data = Vec::new();
        let mut prev: Option<u32> = None;
        let mut data_complete = false;
        for shred in shreds {
            if data_complete {
                return Err(DeshredError::TrailingShreds);
            }
            let view = ShredView::<Data>::read_exact(shred.as_ref())?;
            if let Some(prev) = prev
                && prev.checked_add(1) != Some(view.common.index)
            {
                return Err(DeshredError::NonConsecutive);
            }
            prev = Some(view.common.index);
            let len = kind::data_len(&view.header, view.body.len()).ok_or(
                ParseError::InvalidDataSize {
                    size: view.header.size,
                },
            )?;
            data.extend_from_slice(&view.body[..len]);
            data_complete = view.header.flags.data_complete();
        }
        if !data_complete {
            return Err(DeshredError::Incomplete);
        }
        if data.is_empty() {
            Ok(vec![0u8; 1083])
        } else {
            Ok(data)
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn single_shred_for_tests(slot: Slot, keypair: &Keypair) -> Shred {
        let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 42).unwrap();
        let (mut shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            keypair,
            &[],
            true,
            Hash::default(),
            0,
            &mut ProcessShredsStats::default(),
        );
        shreds.pop().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::shred::{
            CODING_SHREDS_PER_FEC_BLOCK, DATA_SHREDS_PER_FEC_BLOCK, ShredFlags, ShredType,
            max_ticks_per_n_shreds, verify_test_data_shred,
        },
        agave_shred::kind::ShredLayout,
        assert_matches::assert_matches,
        itertools::Itertools,
        rand::Rng,
        solana_hash::Hash,
        solana_pubkey::Pubkey,
        solana_sha256_hasher::hash,
        solana_shred_version as shred_version,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        std::{collections::HashSet, sync::Arc},
        test_case::test_matrix,
    };

    fn verify_test_code_shred(shred: &Shred, index: u32, slot: Slot, pk: &Pubkey, verify: bool) {
        assert!(!shred.is_data());
        assert_eq!(shred.index(), index);
        assert_eq!(shred.slot(), slot);
        let root = shred.merkle_root().unwrap();
        assert_eq!(verify, agave_shred::verify(shred.signature(), pk, &root));
    }

    fn make_entries(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|_| {
                let keypair0 = Keypair::new();
                let keypair1 = Keypair::new();
                let tx0 =
                    system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
                Entry::new(&Hash::default(), 1, vec![tx0])
            })
            .collect()
    }

    fn run_test_data_shredder(slot: Slot, is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());

        assert_matches!(
            Shredder::new(slot, slot + 1, 0, 0),
            Err(BuildError::BadParentSlot { .. })
        );
        assert_matches!(
            Shredder::new(slot, slot - 1 - 0xffff, 0, 0),
            Err(BuildError::BadParentSlot { .. })
        );
        let parent_slot = slot - 5;
        let shredder = Shredder::new(slot, parent_slot, 0, 0).unwrap();
        let entries = make_entries(5);

        let num_expected_data_shreds = DATA_SHREDS_PER_FEC_BLOCK;
        let num_expected_coding_shreds = CODING_SHREDS_PER_FEC_BLOCK;
        let start_index = 0;
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()),
            start_index,
            &mut ProcessShredsStats::default(),
        );
        let next_index = data_shreds.last().unwrap().index() + 1;
        assert_eq!(next_index as usize, num_expected_data_shreds);

        let mut data_shred_indexes = HashSet::new();
        let mut coding_shred_indexes = HashSet::new();
        for shred in data_shreds.iter() {
            assert_eq!(shred.kind(), ShredType::Data);
            let index = shred.index();
            let is_last = index as usize == num_expected_data_shreds - 1;
            verify_test_data_shred(
                shred,
                index,
                slot,
                parent_slot,
                &keypair.pubkey(),
                true,
                is_last && is_last_in_slot,
                is_last,
            );
            assert!(!data_shred_indexes.contains(&index));
            data_shred_indexes.insert(index);
        }

        for shred in coding_shreds.iter() {
            let index = shred.index();
            assert_eq!(shred.kind(), ShredType::Code);
            verify_test_code_shred(shred, index, slot, &keypair.pubkey(), true);
            assert!(!coding_shred_indexes.contains(&index));
            coding_shred_indexes.insert(index);
        }

        for i in start_index..start_index + num_expected_data_shreds as u32 {
            assert!(data_shred_indexes.contains(&i));
        }
        for i in start_index..start_index + num_expected_coding_shreds as u32 {
            assert!(coding_shred_indexes.contains(&i));
        }
        assert_eq!(data_shred_indexes.len(), num_expected_data_shreds);
        assert_eq!(coding_shred_indexes.len(), num_expected_coding_shreds);

        let deshred_payload = Shredder::deshred(data_shreds.iter().map(Shred::bytes)).unwrap();
        let deshred_entries: Vec<Entry> = wincode::deserialize(&deshred_payload).unwrap();
        assert_eq!(entries, deshred_entries);
    }

    #[test_matrix([true, false])]
    fn test_data_shredder(is_last_in_slot: bool) {
        run_test_data_shredder(0x1234_5678_9abc_def0, is_last_in_slot);
    }

    #[test_matrix([true, false])]
    fn test_deserialize_shred_payload(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let shredder = Shredder::new(259_241_705, 259_241_698, 178, 27_471).unwrap();
        let entries = make_entries(5);
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()),
            352,
            &mut ProcessShredsStats::default(),
        );
        for shred in [data_shreds, coding_shreds].into_iter().flatten() {
            let other = Shred::from_blockstore(shred.bytes().clone()).unwrap();
            assert_eq!(shred, other);
        }
    }

    #[test_matrix([true, false])]
    fn test_shred_reference_tick(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let shredder = Shredder::new(1, 0, 5, 0).unwrap();
        let entries = make_entries(5);
        let (data_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()),
            0,
            &mut ProcessShredsStats::default(),
        );
        data_shreds.iter().for_each(|s| {
            assert_eq!(s.reference_tick(), Some(5));
        });
    }

    #[test_matrix([true, false])]
    fn test_shred_reference_tick_overflow(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let shredder = Shredder::new(1, 0, u8::MAX, 0).unwrap();
        let entries = make_entries(5);
        let (data_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()),
            0,
            &mut ProcessShredsStats::default(),
        );
        data_shreds.iter().for_each(|s| {
            assert_eq!(s.reference_tick(), Some(ShredFlags::REFERENCE_TICK_MASK));
        });
    }

    fn run_test_data_and_code_shredder(slot: Slot, is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let shredder = Shredder::new(slot, slot - 5, 0, 0).unwrap();
        let num_entries = max_ticks_per_n_shreds(1, Some(Data::SIZE_OF_BODY)) + 1;
        let entries = make_entries(num_entries as usize);
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()),
            0,
            &mut ProcessShredsStats::default(),
        );
        for (i, s) in data_shreds.iter().enumerate() {
            verify_test_data_shred(
                s,
                s.index(),
                slot,
                slot - 5,
                &keypair.pubkey(),
                true,
                i == data_shreds.len() - 1 && is_last_in_slot,
                i == data_shreds.len() - 1,
            );
        }
        for s in coding_shreds {
            verify_test_code_shred(&s, s.index(), slot, &keypair.pubkey(), true);
        }
    }

    #[test_matrix([true, false])]
    fn test_data_and_code_shredder(is_last_in_slot: bool) {
        run_test_data_and_code_shredder(0x1234_5678_9abc_def0, is_last_in_slot);
    }

    #[test_matrix([true, false])]
    fn test_shred_version(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let hash = hash(Hash::default().as_ref());
        let version = shred_version::version_from_hash(&hash);
        assert_ne!(version, 0);
        let shredder = Shredder::new(0, 0, 0, version).unwrap();
        let entries = make_entries(5);
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()),
            0,
            &mut ProcessShredsStats::default(),
        );
        assert!(
            !data_shreds
                .iter()
                .chain(coding_shreds.iter())
                .any(|s| s.version() != version)
        );
    }

    #[test_matrix([true, false])]
    fn test_components_single_entry_batch_matches_entries(is_last_in_slot: bool) {
        let keypair = Keypair::new();
        let shredder = Shredder::new(100, 95, 5, 42).unwrap();
        let entries = make_entries(10);
        let chained_merkle_root = Hash::new_from_array(rand::rng().random());
        let next_shred_index = 32;

        let (data_shreds_entries, coding_shreds_entries) = shredder
            .entries_to_merkle_shreds_for_tests(
                &keypair,
                &entries,
                is_last_in_slot,
                chained_merkle_root,
                next_shred_index,
                &mut ProcessShredsStats::default(),
            );
        let component = BlockComponent::EntryBatch(entries.clone());
        let (data_shreds_components, coding_shreds_components) = shredder
            .component_to_merkle_shreds_for_tests(
                &keypair,
                &component,
                is_last_in_slot,
                chained_merkle_root,
                next_shred_index,
                &mut ProcessShredsStats::default(),
            );
        assert_eq!(data_shreds_entries, data_shreds_components);
        assert_eq!(coding_shreds_entries, coding_shreds_components);
    }

    #[test_matrix([true, false])]
    fn test_shred_fec_set_index(is_last_in_slot: bool) {
        let keypair = Arc::new(Keypair::new());
        let hash = hash(Hash::default().as_ref());
        let version = shred_version::version_from_hash(&hash);
        let shredder = Shredder::new(0, 0, 0, version).unwrap();
        let entries = make_entries(500);
        let start_index = 0x20;
        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last_in_slot,
            Hash::new_from_array(rand::rng().random()),
            start_index,
            &mut ProcessShredsStats::default(),
        );
        let chunks: Vec<_> = data_shreds
            .iter()
            .chunk_by(|shred| shred.fec_set_index())
            .into_iter()
            .map(|(fec_set_index, chunk)| (fec_set_index, chunk.count()))
            .collect();
        assert!(
            chunks
                .iter()
                .all(|(_, chunk_size)| *chunk_size == DATA_SHREDS_PER_FEC_BLOCK)
        );
        assert_eq!(chunks[0].0, start_index);
        assert!(chunks.iter().tuple_windows().all(
            |((fec_set_index, chunk_size), (next_fec_set_index, _))| fec_set_index
                + *chunk_size as u32
                == *next_fec_set_index
        ));
        assert_eq!(coding_shreds.len(), data_shreds.len());
        assert!(
            coding_shreds
                .iter()
                .zip(&data_shreds)
                .all(|(code, data)| code.fec_set_index() == data.fec_set_index())
        );
    }
}
