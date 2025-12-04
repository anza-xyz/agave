//! Epoch context for transaction execution.

use agave_feature_set::FeatureSet;

/// Epoch-scoped context for transaction execution.
#[derive(Clone, Debug, Default)]
pub struct EpochContext {
    pub features: FeatureSet,
}

#[cfg(feature = "fuzz")]
use super::proto::EpochContext as ProtoEpochContext;

#[cfg(feature = "fuzz")]
impl From<&ProtoEpochContext> for EpochContext {
    fn from(value: &ProtoEpochContext) -> Self {
        Self {
            features: value
                .features
                .as_ref()
                .map(|fs| fs.into())
                .unwrap_or_default(),
        }
    }
}
