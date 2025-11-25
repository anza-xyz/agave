#![allow(clippy::arithmetic_side_effects)]
use {
    solana_clock::Slot,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::shred::{
        self, max_entries_per_n_shred, max_entries_per_n_shred_last_or_not, recover,
        verify_test_data_shred, ProcessShredsStats, ReedSolomonCache, Shred, ShredData, Shredder,
        DATA_SHREDS_PER_FEC_BLOCK,
    },
    solana_signer::Signer,
    solana_system_transaction as system_transaction,
    std::{
        collections::{BTreeMap, HashSet},
        convert::TryInto,
        sync::Arc,
    },
    test_case::test_case,
};

type IndexShredsMap = BTreeMap<u32, Vec<Shred>>;

#[test_case(false)]
#[test_case(true)]
fn test_multi_fec_block_coding(is_last_in_slot: bool) {
    let keypair = Arc::new(Keypair::new());
    let slot = 0x1234_5678_9abc_def0;
    let shredder = Shredder::new(slot, slot - 5, 0, 0).unwrap();
    let num_fec_sets = 100;
    let num_data_shreds = DATA_SHREDS_PER_FEC_BLOCK * num_fec_sets;
    let keypair0 = Keypair::new();
    let keypair1 = Keypair::new();
    let tx0 = system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
    let entry = Entry::new(&Hash::default(), 1, vec![tx0]);
    let num_entries =
        max_entries_per_n_shred_last_or_not(&entry, num_data_shreds as u64, is_last_in_slot);

    let entries: Vec<_> = (0..num_entries)
        .map(|_| {
            let keypair0 = Keypair::new();
            let keypair1 = Keypair::new();
            let tx0 =
                system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
            Entry::new(&Hash::default(), 1, vec![tx0])
        })
        .collect();

    let reed_solomon_cache = ReedSolomonCache::default();
    let serialized_entries = wincode::serialize(&entries).unwrap();

    let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
        &keypair,
        &entries,
        is_last_in_slot,
        Hash::default(), // chained_merkle_root
        0,               // next_shred_index
        0,               // next_code_index
        &reed_solomon_cache,
        &mut ProcessShredsStats::default(),
    );
    let next_index = data_shreds.last().unwrap().index() + 1;
    assert_eq!(next_index as usize, num_data_shreds);
    assert_eq!(data_shreds.len(), num_data_shreds);
    assert_eq!(coding_shreds.len(), num_data_shreds);

    for c in &coding_shreds {
        assert!(!c.is_data());
    }

    let mut all_shreds = vec![];
    for i in 0..num_fec_sets {
        let shred_start_index = DATA_SHREDS_PER_FEC_BLOCK * i;
        let end_index = shred_start_index + DATA_SHREDS_PER_FEC_BLOCK - 1;
        let fec_set_shreds = data_shreds[shred_start_index..=end_index]
            .iter()
            .cloned()
            .chain(coding_shreds[shred_start_index..=end_index].iter().cloned())
            .collect::<Vec<_>>();

        let mut shred_info: Vec<Shred> = fec_set_shreds
            .iter()
            .enumerate()
            .filter_map(|(i, b)| if i % 2 != 0 { Some(b.clone()) } else { None })
            .collect();

        let recovered_data = recover(shred_info.clone(), &reed_solomon_cache)
            .unwrap()
            .map(|result| result.unwrap())
            .filter(|shred| shred.is_data());

        for (i, recovered_shred) in recovered_data.enumerate() {
            let index = shred_start_index + (i * 2);
            verify_test_data_shred(
                &recovered_shred,
                index.try_into().unwrap(),
                slot,
                slot - 5,
                &keypair.pubkey(),
                true,
                index == end_index,
                index == end_index,
            );

            shred_info.insert(i * 2, recovered_shred);
        }

        all_shreds.extend(shred_info.into_iter().take(DATA_SHREDS_PER_FEC_BLOCK));
    }

    let result = {
        let shreds = all_shreds.iter().map(Shred::payload);
        Shredder::deshred(shreds).unwrap()
    };
    assert_eq!(serialized_entries[..], result[..serialized_entries.len()]);
}

#[test]
fn test_multi_fec_block_different_size_coding() {
    let slot = 0x1234_5678_9abc_def0;
    let parent_slot = slot - 5;
    let keypair = Arc::new(Keypair::new());
    let (fec_data, fec_coding, num_shreds_per_iter) =
        setup_different_sized_fec_blocks(slot, parent_slot, keypair.clone());

    let total_num_data_shreds: usize = fec_data.values().map(|x| x.len()).sum();
    let reed_solomon_cache = ReedSolomonCache::default();
    // Test recovery
    for (fec_data_shreds, fec_coding_shreds) in fec_data.values().zip(fec_coding.values()) {
        let first_data_index = fec_data_shreds.first().unwrap().index() as usize;
        let all_shreds: Vec<Shred> = fec_data_shreds
            .iter()
            .step_by(2)
            .chain(fec_coding_shreds.iter().step_by(2))
            .cloned()
            .collect();
        let recovered_data: Vec<Shred> = shred::recover(all_shreds, &reed_solomon_cache)
            .unwrap()
            .filter_map(|s| {
                let s = s.unwrap();
                s.is_data().then_some(s)
            })
            .collect();
        // Necessary in order to ensure the last shred in the slot
        // is part of the recovered set, and that the below `index`
        // calculation in the loop is correct
        assert_eq!(fec_data_shreds.len() % 2, 0);
        for (i, recovered_shred) in recovered_data.into_iter().enumerate() {
            let index = first_data_index + (i * 2) + 1;
            verify_test_data_shred(
                &recovered_shred,
                index.try_into().unwrap(),
                slot,
                parent_slot,
                &keypair.pubkey(),
                true,
                index == total_num_data_shreds - 1,
                index % num_shreds_per_iter == num_shreds_per_iter - 1,
            );
        }
    }
}

fn sort_data_coding_into_fec_sets(
    data_shreds: Vec<Shred>,
    coding_shreds: Vec<Shred>,
    fec_data: &mut IndexShredsMap,
    fec_coding: &mut IndexShredsMap,
    data_slot_and_index: &mut HashSet<(Slot, u32)>,
    coding_slot_and_index: &mut HashSet<(Slot, u32)>,
) {
    for shred in data_shreds {
        assert!(shred.is_data());
        let key = (shred.slot(), shred.index());
        // Make sure there are no duplicates for same key
        assert!(!data_slot_and_index.contains(&key));
        data_slot_and_index.insert(key);
        let fec_entry = fec_data.entry(shred.fec_set_index()).or_default();
        fec_entry.push(shred);
    }
    for shred in coding_shreds {
        assert!(!shred.is_data());
        let key = (shred.slot(), shred.index());
        // Make sure there are no duplicates for same key
        assert!(!coding_slot_and_index.contains(&key));
        coding_slot_and_index.insert(key);
        let fec_entry = fec_coding.entry(shred.fec_set_index()).or_default();
        fec_entry.push(shred);
    }
}

#[allow(clippy::assertions_on_constants)]
fn setup_different_sized_fec_blocks(
    slot: Slot,
    parent_slot: Slot,
    keypair: Arc<Keypair>,
) -> (IndexShredsMap, IndexShredsMap, usize) {
    let shredder = Shredder::new(slot, parent_slot, 0, 0).unwrap();
    let keypair0 = Keypair::new();
    let keypair1 = Keypair::new();
    let tx0 = system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
    let entry = Entry::new(&Hash::default(), 1, vec![tx0]);
    let merkle_capacity = ShredData::capacity(/*proof_size:*/ 6, /*resigned:*/ true).unwrap();
    let chained_merkle_root = Hash::default();

    assert!(DATA_SHREDS_PER_FEC_BLOCK > 2);
    let num_shreds_per_iter = DATA_SHREDS_PER_FEC_BLOCK;
    let num_entries =
        max_entries_per_n_shred(&entry, num_shreds_per_iter as u64, Some(merkle_capacity));

    let entries: Vec<_> = (0..num_entries)
        .map(|_| {
            let keypair0 = Keypair::new();
            let keypair1 = Keypair::new();
            let tx0 =
                system_transaction::transfer(&keypair0, &keypair1.pubkey(), 1, Hash::default());
            Entry::new(&Hash::default(), 1, vec![tx0])
        })
        .collect();

    // Run the shredder twice, generate data and coding shreds
    let mut next_shred_index = 0;
    let mut next_code_index = 0;
    let mut fec_data = BTreeMap::new();
    let mut fec_coding = BTreeMap::new();
    let mut data_slot_and_index = HashSet::new();
    let mut coding_slot_and_index = HashSet::new();

    let total_num_data_shreds: usize = 2 * num_shreds_per_iter;
    let reed_solomon_cache = ReedSolomonCache::default();
    for i in 0..2 {
        let is_last = i == 1;

        let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &entries,
            is_last,
            chained_merkle_root,
            next_shred_index,
            next_code_index,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
        for shred in &data_shreds {
            if (shred.index() as usize) == total_num_data_shreds - 1 {
                assert!(shred.data_complete());
                assert!(shred.last_in_slot());
            } else if (shred.index() as usize) % num_shreds_per_iter == num_shreds_per_iter - 1 {
                assert!(shred.data_complete());
            } else {
                assert!(!shred.data_complete());
                assert!(!shred.last_in_slot());
            }
        }
        assert_eq!(data_shreds.len(), num_shreds_per_iter);
        next_shred_index = data_shreds.last().unwrap().index() + 1;
        next_code_index = coding_shreds.last().unwrap().index() + 1;
        sort_data_coding_into_fec_sets(
            data_shreds,
            coding_shreds,
            &mut fec_data,
            &mut fec_coding,
            &mut data_slot_and_index,
            &mut coding_slot_and_index,
        );
    }

    assert_eq!(fec_data.len(), fec_coding.len());
    (fec_data, fec_coding, num_shreds_per_iter)
}
