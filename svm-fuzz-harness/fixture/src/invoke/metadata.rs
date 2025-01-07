//! Instruction metadata.

use {crate::proto::FixtureMetadata as ProtoFixtureMetadata, solana_keccak_hasher::Hasher};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FixtureMetadata {
    /// The program entrypoint function name.
    pub entrypoint: String,
}

impl From<ProtoFixtureMetadata> for FixtureMetadata {
    fn from(value: ProtoFixtureMetadata) -> Self {
        Self {
            entrypoint: value.fn_entrypoint,
        }
    }
}

impl From<FixtureMetadata> for ProtoFixtureMetadata {
    fn from(value: FixtureMetadata) -> Self {
        Self {
            fn_entrypoint: value.entrypoint,
        }
    }
}

pub(crate) fn hash_proto_fixture_metadata(hasher: &mut Hasher, metadata: &ProtoFixtureMetadata) {
    hasher.hash(metadata.fn_entrypoint.as_bytes());
}
