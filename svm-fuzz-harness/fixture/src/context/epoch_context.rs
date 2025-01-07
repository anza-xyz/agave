//! Context for an epoch.

use {crate::proto::EpochContext as ProtoEpochContext, solana_feature_set::FeatureSet};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EpochContext {
    /// The feature set to use for the simulation.
    pub feature_set: FeatureSet,
}

impl From<ProtoEpochContext> for EpochContext {
    fn from(value: ProtoEpochContext) -> Self {
        Self {
            feature_set: value.features.map(Into::into).unwrap_or_default(),
        }
    }
}

impl From<EpochContext> for ProtoEpochContext {
    fn from(value: EpochContext) -> Self {
        Self {
            features: Some(value.feature_set.into()),
        }
    }
}
