//! Solana SVM instruction fixture.

pub mod context;
pub mod effects;
pub mod instr_account;
pub mod metadata;

use {
    crate::{error::FixtureError, proto::InstrFixture as ProtoInstrFixture},
    context::InstrContext,
    effects::InstrEffects,
    metadata::FixtureMetadata,
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
