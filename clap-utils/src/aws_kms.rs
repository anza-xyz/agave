//! A [`Signer`] backed by an Ed25519 key held in AWS KMS.
//!
//! The private key never leaves KMS: the public key is read once with
//! `GetPublicKey`, and every message is signed with `Sign` using
//! `ED25519_SHA_512` over the raw message bytes (pure EdDSA), then verified
//! locally against that public key.

use {
    aws_sdk_kms::{
        Client,
        error::DisplayErrorContext,
        primitives::Blob,
        types::{KeySpec, MessageType, SigningAlgorithmSpec},
    },
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::{Signer, SignerError},
    std::future::Future,
};

/// DER prefix of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410); the 32-byte key follows it.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// Location of an AWS KMS key, as given in an `aws-kms:` signer source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsKmsKey {
    /// Key ID, key ARN, alias name (`alias/...`) or alias ARN.
    pub key_id: String,
    /// Region to call. When `None`, the region of an ARN is used, then the
    /// default AWS configuration (`AWS_REGION`, profile).
    pub region: Option<String>,
}

impl AwsKmsKey {
    /// Parses the part of a signer source after `aws-kms:`, with or without a
    /// leading `//`, e.g. `alias/treasury?region=us-east-1`.
    pub(crate) fn parse(source: &str) -> Option<Self> {
        let source = source.strip_prefix("//").unwrap_or(source);
        let (key_id, query) = match source.split_once('?') {
            Some((key_id, query)) => (key_id, Some(query)),
            None => (source, None),
        };
        if key_id.is_empty() {
            return None;
        }
        let mut region = None;
        for pair in query.into_iter().flat_map(|query| query.split('&')) {
            match pair.split_once('=') {
                Some(("region", value)) if !value.is_empty() => region = Some(value.to_string()),
                _ => return None,
            }
        }
        let region = region.or_else(|| region_from_arn(key_id));
        Some(Self {
            key_id: key_id.to_string(),
            region,
        })
    }
}

/// Returns the region of a KMS key or alias ARN (`arn:aws:kms:<region>:...`).
fn region_from_arn(key_id: &str) -> Option<String> {
    let mut parts = key_id.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("arn"), Some(_partition), Some("kms"), Some(region)) if !region.is_empty() => {
            Some(region.to_string())
        }
        _ => None,
    }
}

fn pubkey_from_spki(der: &[u8]) -> Option<Pubkey> {
    der.strip_prefix(&ED25519_SPKI_PREFIX)
        .and_then(|key| <[u8; 32]>::try_from(key).ok())
        .map(Pubkey::new_from_array)
}

/// Runs an AWS SDK call to completion on its own thread and runtime.
///
/// [`Signer`] is synchronous, but it is also called from inside the async
/// `solana` CLI, where blocking on the caller's runtime would panic.
fn run<T, F, Fut>(f: F) -> Result<T, SignerError>
where
    T: Send,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<T, SignerError>>,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| SignerError::Custom(format!("AWS KMS runtime: {err}")))?
                    .block_on(f())
            })
            .join()
            .map_err(|_| SignerError::Custom("AWS KMS request panicked".to_string()))?
    })
}

async fn client(region: Option<&str>) -> Client {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = region {
        loader = loader.region(aws_config::Region::new(region.to_string()));
    }
    Client::new(&loader.load().await)
}

pub struct AwsKmsSigner {
    key: AwsKmsKey,
    pubkey: Pubkey,
}

impl AwsKmsSigner {
    /// Reads the key's public key from KMS and checks that it is an Ed25519 key.
    pub fn new(key: AwsKmsKey) -> Result<Self, SignerError> {
        let pubkey = run(|| async {
            let output = client(key.region.as_deref())
                .await
                .get_public_key()
                .key_id(&key.key_id)
                .send()
                .await
                .map_err(|err| {
                    SignerError::Connection(format!(
                        "AWS KMS GetPublicKey for {}: {}",
                        key.key_id,
                        DisplayErrorContext(&err)
                    ))
                })?;
            if output.key_spec() != Some(&KeySpec::EccNistEdwards25519) {
                return Err(SignerError::InvalidInput(format!(
                    "AWS KMS key {} is {:?}, not an Ed25519 (ECC_NIST_EDWARDS25519) key",
                    key.key_id,
                    output.key_spec()
                )));
            }
            output
                .public_key()
                .and_then(|der| pubkey_from_spki(der.as_ref()))
                .ok_or_else(|| {
                    SignerError::Protocol(format!(
                        "AWS KMS key {} returned an unexpected public key encoding",
                        key.key_id
                    ))
                })
        })?;
        Ok(Self { key, pubkey })
    }
}

impl Signer for AwsKmsSigner {
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        Ok(self.pubkey)
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        let bytes = run(|| async {
            client(self.key.region.as_deref())
                .await
                .sign()
                .key_id(&self.key.key_id)
                .message(Blob::new(message))
                .message_type(MessageType::Raw)
                .signing_algorithm(SigningAlgorithmSpec::Ed25519Sha512)
                .send()
                .await
                .map_err(|err| {
                    SignerError::Connection(format!(
                        "AWS KMS Sign with {}: {}",
                        self.key.key_id,
                        DisplayErrorContext(&err)
                    ))
                })?
                .signature
                .map(Blob::into_inner)
                .ok_or_else(|| SignerError::Protocol("AWS KMS returned no signature".to_string()))
        })?;
        let signature = Signature::try_from(bytes.as_slice()).map_err(|_| {
            SignerError::Protocol(format!(
                "AWS KMS returned a {}-byte signature, expected 64",
                bytes.len()
            ))
        })?;
        if !signature.verify(self.pubkey.as_ref(), message) {
            return Err(SignerError::Protocol(format!(
                "AWS KMS signature from {} does not verify against {}",
                self.key.key_id, self.pubkey
            )));
        }
        Ok(signature)
    }

    fn is_interactive(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_aws_kms_key() {
        let key_id = "1234abcd-12ab-34cd-56ef-1234567890ab";
        for source in [key_id.to_string(), format!("//{key_id}")] {
            assert_eq!(
                AwsKmsKey::parse(&source),
                Some(AwsKmsKey {
                    key_id: key_id.to_string(),
                    region: None,
                })
            );
        }
        assert_eq!(
            AwsKmsKey::parse("//alias/treasury?region=eu-west-1"),
            Some(AwsKmsKey {
                key_id: "alias/treasury".to_string(),
                region: Some("eu-west-1".to_string()),
            })
        );
        let arn = format!("arn:aws:kms:us-east-2:111122223333:key/{key_id}");
        assert_eq!(
            AwsKmsKey::parse(&arn),
            Some(AwsKmsKey {
                key_id: arn.clone(),
                region: Some("us-east-2".to_string()),
            })
        );
        // An explicit region wins over the ARN's.
        assert_eq!(
            AwsKmsKey::parse(&format!("{arn}?region=us-west-2"))
                .unwrap()
                .region,
            Some("us-west-2".to_string())
        );
        assert_eq!(AwsKmsKey::parse(""), None);
        assert_eq!(AwsKmsKey::parse("//"), None);
        assert_eq!(AwsKmsKey::parse("alias/treasury?profile=dev"), None);
        assert_eq!(AwsKmsKey::parse("alias/treasury?region="), None);
    }

    #[test]
    fn test_pubkey_from_spki() {
        let key = [7u8; 32];
        let mut der = ED25519_SPKI_PREFIX.to_vec();
        der.extend_from_slice(&key);
        assert_eq!(pubkey_from_spki(&der), Some(Pubkey::new_from_array(key)));
        assert_eq!(pubkey_from_spki(&der[..der.len() - 1]), None);
        der[8] = 0x71; // Ed448 OID
        assert_eq!(pubkey_from_spki(&der), None);
    }

    /// Signs with a real KMS key: `AWS_KMS_TEST_KEY_ID=alias/... cargo test -p solana-clap-utils
    /// --features agave-unstable-api,aws-kms -- --ignored aws_kms`
    #[test]
    #[ignore]
    fn test_aws_kms_signer_live() {
        let key_id = std::env::var("AWS_KMS_TEST_KEY_ID").expect("AWS_KMS_TEST_KEY_ID");
        let signer = AwsKmsSigner::new(AwsKmsKey::parse(&key_id).unwrap()).unwrap();
        let message = b"hello from solana-clap-utils";
        let signature = signer.try_sign_message(message).unwrap();
        assert!(signature.verify(signer.pubkey().as_ref(), message));
        // Signing works from inside a runtime too, as in the `solana` CLI.
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { signer.try_sign_message(message) })
            .unwrap();
    }
}
