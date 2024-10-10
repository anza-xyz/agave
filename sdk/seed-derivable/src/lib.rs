//! The interface by which keys are derived.
use {
    ed25519_dalek_bip32::Error as Bip32Error,
    solana_derivation_path::DerivationPath,
    solana_seed_phrase::keypair_from_seed_phrase_and_passphrase,
    solana_signer::keypair::{keypair_from_seed, Keypair},
    std::error,
};

/// The `SeedDerivable` trait defines the interface by which cryptographic keys/keypairs are
/// derived from byte seeds, derivation paths, and passphrases.
pub trait SeedDerivable: Sized {
    fn from_seed(seed: &[u8]) -> Result<Self, Box<dyn error::Error>>;
    fn from_seed_and_derivation_path(
        seed: &[u8],
        derivation_path: Option<DerivationPath>,
    ) -> Result<Self, Box<dyn error::Error>>;
    fn from_seed_phrase_and_passphrase(
        seed_phrase: &str,
        passphrase: &str,
    ) -> Result<Self, Box<dyn error::Error>>;
}

impl SeedDerivable for Keypair {
    fn from_seed(seed: &[u8]) -> Result<Self, Box<dyn error::Error>> {
        keypair_from_seed(seed)
    }

    fn from_seed_and_derivation_path(
        seed: &[u8],
        derivation_path: Option<DerivationPath>,
    ) -> Result<Self, Box<dyn error::Error>> {
        keypair_from_seed_and_derivation_path(seed, derivation_path)
    }

    fn from_seed_phrase_and_passphrase(
        seed_phrase: &str,
        passphrase: &str,
    ) -> Result<Self, Box<dyn error::Error>> {
        keypair_from_seed_phrase_and_passphrase(seed_phrase, passphrase)
    }
}

/// Generates a Keypair using Bip32 Hierarchical Derivation if derivation-path is provided;
/// otherwise generates the base Bip44 Solana keypair from the seed
pub fn keypair_from_seed_and_derivation_path(
    seed: &[u8],
    derivation_path: Option<DerivationPath>,
) -> Result<Keypair, Box<dyn error::Error>> {
    let derivation_path = derivation_path.unwrap_or_default();
    bip32_derived_keypair(seed, derivation_path).map_err(|err| err.to_string().into())
}

/// Generates a Keypair using Bip32 Hierarchical Derivation
fn bip32_derived_keypair(
    seed: &[u8],
    derivation_path: DerivationPath,
) -> Result<Keypair, Bip32Error> {
    let extended = ed25519_dalek_bip32::ExtendedSecretKey::from_seed(seed)
        .and_then(|extended| extended.derive(&derivation_path))?;
    let extended_public_key = extended.public_key();
    Ok(Keypair::from(ed25519_dalek::Keypair {
        secret: extended.secret_key,
        public: extended_public_key,
    }))
}
