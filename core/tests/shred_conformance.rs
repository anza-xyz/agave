#![cfg(feature = "conformance")]

use {
    prost::Message,
    protosol::protos::{BlockParseResult, ShredParseContext, ShredParseEffects, ShredParseFixture},
    solana_core::conformance::shred::{execute_shred_parse, sol_compat_shred_parse_v1},
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::shred::{
        DATA_SHREDS_PER_FEC_BLOCK, ProcessShredsStats, ReedSolomonCache, Shredder,
        max_entries_per_n_shred_last_or_not,
    },
    solana_signer::Signer,
    std::{mem::MaybeUninit, path::PathBuf, sync::Arc},
};

fn context_with_shreds(shreds: Vec<Vec<u8>>) -> ShredParseContext {
    context_at_root(1, shreds)
}

fn context_at_root(root_slot: u64, shreds: Vec<Vec<u8>>) -> ShredParseContext {
    ShredParseContext {
        shreds,
        root_slot,
        shred_version: 0,
        features: None,
    }
}

#[test]
fn test_empty_context_is_accepted() {
    let effects = execute_shred_parse(&context_with_shreds(vec![]));

    assert_eq!(
        effects.block_parse_result,
        BlockParseResult::Accepted as i32
    );
    assert!(effects.shred_results.is_empty());
    assert!(effects.fec_set_results.is_empty());
}

#[test]
fn test_malformed_shred_does_not_parse() {
    let effects = execute_shred_parse(&context_with_shreds(vec![vec![0; 8]]));

    assert_eq!(
        effects.block_parse_result,
        BlockParseResult::Accepted as i32
    );
    assert_eq!(effects.shred_results, [false]);
    assert!(effects.fec_set_results.is_empty());
}

fn make_recoverable_fec_set() -> (Vec<Vec<u8>>, Vec<u8>) {
    make_recoverable_fec_set_at(2, 1)
}

fn make_recoverable_fec_set_at(slot: u64, parent: u64) -> (Vec<Vec<u8>>, Vec<u8>) {
    let keypair = Arc::new(Keypair::new());
    let shredder = Shredder::new(slot, parent, 0, 0).unwrap();
    let sender = Keypair::new();
    let recipient = Keypair::new();
    let transaction =
        solana_system_transaction::transfer(&sender, &recipient.pubkey(), 1, Hash::default());
    let entry = Entry::new(&Hash::default(), 1, vec![transaction]);
    let num_entries =
        max_entries_per_n_shred_last_or_not(&entry, DATA_SHREDS_PER_FEC_BLOCK as u64, false);
    let entries = vec![entry; num_entries.try_into().unwrap()];
    let (data_shreds, coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
        &keypair,
        &entries,
        false,
        Hash::default(),
        0,
        0,
        &ReedSolomonCache::default(),
        &mut ProcessShredsStats::default(),
    );
    assert_eq!(data_shreds.len(), DATA_SHREDS_PER_FEC_BLOCK);
    assert_eq!(coding_shreds.len(), DATA_SHREDS_PER_FEC_BLOCK);

    let incomplete_shred = data_shreds[0].payload().to_vec();
    let recoverable = data_shreds
        .into_iter()
        .take(DATA_SHREDS_PER_FEC_BLOCK / 2)
        .chain(
            coding_shreds
                .into_iter()
                .take(DATA_SHREDS_PER_FEC_BLOCK / 2),
        )
        .map(|shred| shred.into_payload().to_vec())
        .collect();
    (recoverable, incomplete_shred)
}

#[test]
fn test_recovers_fec_set_and_resets_cached_blockstore() {
    let (recoverable, incomplete_shred) = make_recoverable_fec_set();
    let effects = execute_shred_parse(&context_with_shreds(recoverable));

    assert_eq!(
        effects.block_parse_result,
        BlockParseResult::Accepted as i32
    );
    assert_eq!(effects.fec_set_results.len(), 1);
    let fec_set = &effects.fec_set_results[0];
    assert!(fec_set.completed);
    assert_eq!(fec_set.slot, 2);
    assert_eq!(fec_set.fec_set_index, 0);
    assert_eq!(fec_set.num_data_shreds, DATA_SHREDS_PER_FEC_BLOCK as u32);
    assert_eq!(fec_set.num_coding_shreds, 32);
    assert_eq!(fec_set.merkle_root.len(), 32);
    assert_eq!(fec_set.chained_merkle_root.len(), 32);
    assert!(!fec_set.payload.is_empty());

    let effects = execute_shred_parse(&context_with_shreds(vec![incomplete_shred]));
    assert!(
        effects.fec_set_results.is_empty(),
        "cached shreds leaked into the next input"
    );
}

#[test]
fn test_slot_max_does_not_leak_from_cached_blockstore() {
    let (recoverable, incomplete_shred) = make_recoverable_fec_set_at(u64::MAX, u64::MAX - 1);
    let effects = execute_shred_parse(&context_at_root(u64::MAX - 1, recoverable));
    assert_eq!(effects.fec_set_results.len(), 1);

    let effects = execute_shred_parse(&context_at_root(u64::MAX - 1, vec![incomplete_shred]));
    assert!(
        effects.fec_set_results.is_empty(),
        "slot u64::MAX leaked into the next input"
    );
}

fn encoded_nonempty_context() -> Vec<u8> {
    context_with_shreds(vec![vec![0; 8]]).encode_to_vec()
}

#[test]
fn test_shred_ffi_round_trip() {
    let mut input = encoded_nonempty_context();
    let mut output = vec![MaybeUninit::<u8>::uninit(); 1024];
    let mut output_size = output.len() as u64;

    let status = unsafe {
        sol_compat_shred_parse_v1(
            output.as_mut_ptr().cast(),
            &mut output_size,
            input.as_mut_ptr(),
            input.len() as u64,
        )
    };

    assert_eq!(status, 1);
    let output =
        unsafe { std::slice::from_raw_parts(output.as_ptr().cast(), output_size as usize) };
    let effects = ShredParseEffects::decode(output).unwrap();
    assert_eq!(effects.shred_results, [false]);
}

#[test]
fn test_shred_ffi_rejects_malformed_input_and_small_output() {
    let mut malformed = vec![0xff];
    let mut output = vec![0; 1024];
    let mut output_size = output.len() as u64;
    let status = unsafe {
        sol_compat_shred_parse_v1(
            output.as_mut_ptr(),
            &mut output_size,
            malformed.as_mut_ptr(),
            malformed.len() as u64,
        )
    };
    assert_eq!(status, 0);

    let mut input = encoded_nonempty_context();
    let mut output_size = 0;
    let status = unsafe {
        sol_compat_shred_parse_v1(
            output.as_mut_ptr(),
            &mut output_size,
            input.as_mut_ptr(),
            input.len() as u64,
        )
    };
    assert_eq!(status, 0);
    assert_eq!(output_size, 0);
}

#[test]
fn test_shred_ffi_rejects_null_pointers_and_empty_input() {
    let mut input = encoded_nonempty_context();
    let mut output = vec![0; 1024];
    let mut output_size = output.len() as u64;

    assert_eq!(
        unsafe {
            sol_compat_shred_parse_v1(
                std::ptr::null_mut(),
                &mut output_size,
                input.as_mut_ptr(),
                input.len() as u64,
            )
        },
        0
    );
    assert_eq!(
        unsafe {
            sol_compat_shred_parse_v1(
                output.as_mut_ptr(),
                std::ptr::null_mut(),
                input.as_mut_ptr(),
                input.len() as u64,
            )
        },
        0
    );
    assert_eq!(
        unsafe {
            sol_compat_shred_parse_v1(
                output.as_mut_ptr(),
                &mut output_size,
                std::ptr::null_mut(),
                input.len() as u64,
            )
        },
        0
    );
    assert_eq!(
        unsafe {
            sol_compat_shred_parse_v1(output.as_mut_ptr(), &mut output_size, input.as_mut_ptr(), 0)
        },
        0
    );
}

#[test]
#[ignore = "requires an external shred fixture corpus"]
fn test_shred_fixture_corpus() {
    let fixture_dir = PathBuf::from(
        std::env::var("SHRED_FIXTURE_DIR").expect("SHRED_FIXTURE_DIR must point to shred/fixtures"),
    );
    let mut fixture_paths: Vec<_> = std::fs::read_dir(&fixture_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", fixture_dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "fix"))
        .collect();
    fixture_paths.sort_unstable();
    assert!(!fixture_paths.is_empty(), "no shred fixtures found");

    let mut mismatches = Vec::new();
    let mut first_mismatch = None;
    for path in &fixture_paths {
        let bytes = std::fs::read(path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let fixture = ShredParseFixture::decode(bytes.as_slice())
            .unwrap_or_else(|err| panic!("failed to decode {}: {err}", path.display()));
        let input = fixture
            .input
            .unwrap_or_else(|| panic!("missing input in {}", path.display()));
        let expected = fixture
            .output
            .unwrap_or_else(|| panic!("missing output in {}", path.display()));
        let actual = execute_shred_parse(&input);
        if actual != expected {
            mismatches.push(path.display().to_string());
            first_mismatch.get_or_insert_with(|| {
                format!(
                    "{}\nexpected: {expected:?}\nactual: {actual:?}",
                    path.display()
                )
            });
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} of {} shred fixtures mismatched:\n{}\n\nfirst mismatch:\n{}",
        mismatches.len(),
        fixture_paths.len(),
        mismatches.join("\n"),
        first_mismatch.unwrap_or_default(),
    );
}
