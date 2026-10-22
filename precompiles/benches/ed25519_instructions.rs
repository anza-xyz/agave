use {
    agave_feature_set::FeatureSet,
    agave_precompiles::ed25519::verify,
    criterion::{Criterion, criterion_group, criterion_main},
    ed25519_dalek::ed25519::signature::Signer,
    rand::Rng,
    solana_ed25519_program::new_ed25519_instruction_with_signature,
    solana_instruction::Instruction,
    std::time::Duration,
};

// Cap the corpus by total message bytes rather than instruction count: the
// large-message cases otherwise allocate hundreds of MiB and spend most of the
// setup signing it, without verifying anything extra.
const IX_BYTES_BUDGET: usize = 32 << 20;
const IX_COUNT_MAX: usize = 1024;

// prepare a bunch of unique txs
fn create_test_instructions(message_length: u16) -> Vec<Instruction> {
    let mut rng = rand::rng();
    let ix_count = IX_BYTES_BUDGET
        .checked_div(usize::from(message_length))
        .unwrap_or(IX_COUNT_MAX)
        .clamp(1, IX_COUNT_MAX);
    (0..ix_count)
        .map(|_| {
            let secret_bytes: [u8; 32] = rand::random();
            let secret = ed25519_dalek::SecretKey::from_bytes(&secret_bytes).unwrap();
            let public: ed25519_dalek::PublicKey = (&secret).into();
            let privkey = ed25519_dalek::Keypair { secret, public };
            let mut message = vec![0u8; usize::from(message_length)];
            rng.fill(message.as_mut_slice());
            let signature = privkey.sign(&message).to_bytes();
            let pubkey = privkey.public.to_bytes();
            new_ed25519_instruction_with_signature(&message, &signature, &pubkey)
        })
        .collect()
}

fn bench_verify(c: &mut Criterion, name: &str, message_length: u16) {
    let feature_set = FeatureSet::all_enabled();
    let ixs = create_test_instructions(message_length);
    let mut ix_iter = ixs.iter().cycle();
    c.bench_function(name, |b| {
        b.iter(|| {
            let instruction = ix_iter.next().unwrap();
            verify(&instruction.data, &[&instruction.data], &feature_set).unwrap();
        })
    });
}

fn bench_ed25519(c: &mut Criterion) {
    let required_extra_space = 113_u16; // len for pubkey, sig, and offsets
    bench_verify(c, "bench_ed25519_len_032", 32);
    bench_verify(c, "bench_ed25519_len_128", 128);
    bench_verify(c, "bench_ed25519_len_32k", 32 * 1024);
    bench_verify(c, "bench_ed25519_len_max", u16::MAX - required_extra_space);
}

criterion_group! {
    name = benches;
    // Trim criterion's defaults to keep the suite near its libtest wall time.
    // A single verify is tens of microseconds, so these windows still fit
    // thousands of iterations per sample.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(750))
        .sample_size(20)
        .without_plots();
    targets = bench_ed25519
}
criterion_main!(benches);
