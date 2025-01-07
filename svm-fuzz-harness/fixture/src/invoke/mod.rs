//! Solana SVM instruction fixture.

pub mod context;
pub mod effects;
pub mod instr_account;
pub mod metadata;

#[cfg(feature = "serde")]
use solana_svm_fuzz_harness_fixture_fs::FsHandler;
use {
    crate::{error::FixtureError, proto::InstrFixture as ProtoInstrFixture},
    context::InstrContext,
    effects::InstrEffects,
    metadata::FixtureMetadata,
    solana_keccak_hasher::{Hash, Hasher},
    solana_svm_fuzz_harness_fixture_fs::{IntoSerializableFixture, SerializableFixture},
};

/// A fixture for invoking a single instruction against a simulated SVM
/// program runtime environment, for a given program.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InstrFixture {
    /// The fixture metadata.
    pub metadata: Option<FixtureMetadata>,
    /// The fixture inputs.
    pub input: InstrContext,
    /// The fixture outputs.
    pub output: InstrEffects,
}

impl InstrFixture {
    pub fn decode(blob: &[u8]) -> Result<Self, FixtureError> {
        let proto_fixture = <ProtoInstrFixture as SerializableFixture>::decode(blob)?;
        proto_fixture.try_into()
    }

    #[cfg(feature = "serde")]
    pub fn load_from_blob_file(file_path: &str) -> Result<Self, FixtureError> {
        let proto_fixture: ProtoInstrFixture = FsHandler::load_from_blob_file(file_path);
        proto_fixture.try_into()
    }

    #[cfg(feature = "serde")]
    pub fn load_from_json_file(file_path: &str) -> Result<Self, FixtureError> {
        let proto_fixture: ProtoInstrFixture = FsHandler::load_from_json_file(file_path);
        proto_fixture.try_into()
    }
}

impl TryFrom<ProtoInstrFixture> for InstrFixture {
    type Error = FixtureError;

    fn try_from(value: ProtoInstrFixture) -> Result<Self, Self::Error> {
        // All blobs should have an input and output.
        Ok(Self {
            metadata: value.metadata.map(Into::into),
            input: value.input.unwrap().try_into()?,
            output: value.output.unwrap().try_into()?,
        })
    }
}

impl From<InstrFixture> for ProtoInstrFixture {
    fn from(value: InstrFixture) -> Self {
        Self {
            metadata: value.metadata.map(Into::into),
            input: Some(value.input.into()),
            output: Some(value.output.into()),
        }
    }
}

impl SerializableFixture for ProtoInstrFixture {
    // Manually implemented for deterministic hashes.
    fn hash(&self) -> Hash {
        let mut hasher = Hasher::default();
        if let Some(metadata) = &self.metadata {
            metadata::hash_proto_fixture_metadata(&mut hasher, metadata);
        }
        if let Some(input) = &self.input {
            context::hash_proto_instr_context(&mut hasher, input);
        }
        if let Some(output) = &self.output {
            effects::hash_proto_instr_effects(&mut hasher, output);
        }
        hasher.result()
    }
}

impl IntoSerializableFixture for InstrFixture {
    type Fixture = ProtoInstrFixture;

    fn into(self) -> Self::Fixture {
        Into::into(self)
    }
}
