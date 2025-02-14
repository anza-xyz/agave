//! Contains fixtures for tests of gossip code
//! Some of these are cryptographically unsound and
//! do not belong in any prod code.
#[cfg(not(feature = "dev-context-only-utils"))]
use non_existent_crate_to_make_sure_this_is_never_imported_in_prod;
use {
    anyhow::Context,
    rand::Rng,
    rand0_7::{CryptoRng as CryptoRng0_7, RngCore as RngCore0_7},
    solana_pubkey::{Pubkey, PUBKEY_BYTES},
    solana_sanitize::Sanitize,
    solana_sdk::{signature::Keypair, timing::timestamp},
};

pub trait FormatValidation<SourceMaterial = Pubkey>: Sized + Sanitize {
    /// New random instance for tests and benchmarks.
    /// Implementations may customize which source material they need to build an instance.
    fn new_rand<R: Rng>(rng: &mut R, source_material: Option<SourceMaterial>) -> Self;

    /// Run some code that uses/traverses the value in a meaningful way
    /// This is primarily designed for use in fuzz tests.
    fn exercise(&self) -> anyhow::Result<()> {
        self.sanitize().context("Sanitize failed")?;
        Ok(())
    }
}

/// Random gossip timestamp for tests and benchmarks.
/// With seeded rng can be repeatable
pub fn new_rand_timestamp<R: Rng>(rng: &mut R) -> u64 {
    const DELAY: u64 = 10 * 60 * 1000; // 10 minutes
    timestamp() - DELAY + rng.gen_range(0..2 * DELAY)
}

/// *Insecurely* generated keypair. Designed for tests and benches.
/// Unlike a proper keypair generation, this can use any rng.
/// With seeded rng can be repeatable
pub fn new_insecure_keypair<R: Rng>(rng: &mut R) -> Keypair {
    let mut not_crypto_rng = FakeCryptoRng::new(rng);
    Keypair::generate(&mut not_crypto_rng)
}

/// Random public key bytes. Designed for tests and benches.
/// With seeded rng can be repeatable
pub fn new_random_pubkey<R: Rng>(rng: &mut R) -> Pubkey {
    let bytes = rng.gen::<[u8; PUBKEY_BYTES]>();
    Pubkey::from(bytes)
}

/// Fake crypto RNG that can make signatures using any underlying RNG
/// Obviously, this is for tests and benchmakrs only
struct FakeCryptoRng<'r, R: Rng>(&'r mut R);
/// Marking the `FakeCryptoRng` as crypto RNG for tests only
impl<R: Rng> CryptoRng0_7 for FakeCryptoRng<'_, R> {}

impl<'r, R: Rng> FakeCryptoRng<'r, R> {
    //just in case, prevent construction of instances outside of tests
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new(rng: &'r mut R) -> Self {
        Self(rng)
    }
}

impl<R: Rng> RngCore0_7 for FakeCryptoRng<'_, R> {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand0_7::Error> {
        self.0.try_fill_bytes(dest).map_err(rand0_7::Error::new)
    }
}
