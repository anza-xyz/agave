use {
    clap::ArgMatches,
    solana_clap_v3_utils::input_parsers::{
        pubkeys_of_multiple_signers,
        signer::{try_keypairs_of, try_pubkeys_of, try_pubkeys_sigs_of, SignerSource},
    },
};

// Ensures these functions have graceful handling of absent arguments

#[test]
fn test_try_pubkeys_of() {
    let matches = ArgMatches::default();
    let result = try_pubkeys_of(&matches, "test").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_try_pubkeys_sigs_of() {
    let matches = ArgMatches::default();
    let result = try_pubkeys_sigs_of(&matches, "test").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_pubkeys_of_multiple_signers() {
    let matches = ArgMatches::default();
    let result = pubkeys_of_multiple_signers(&matches, "test", &mut None).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_try_keypairs_of() {
    let matches = ArgMatches::default();
    let result = try_keypairs_of(&matches, "test").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_try_get_keypairs() {
    let matches = ArgMatches::default();
    let result = SignerSource::try_get_keypairs(&matches, "test").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_try_get_signers() {
    let matches = ArgMatches::default();
    let result = SignerSource::try_get_signers(&matches, "test", &mut None).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_try_get_pubkeys() {
    let matches = ArgMatches::default();
    let result = SignerSource::try_get_pubkeys(&matches, "test", &mut None).unwrap();
    assert!(result.is_none());
}
