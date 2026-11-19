//! Feature set conversions for protobuf support.

use {
    agave_feature_set::{FEATURE_NAMES, FeatureSet},
    ahash::{AHashMap, AHashSet},
    protosol::protos::FeatureSet as ProtoFeatureSet,
    solana_pubkey::Pubkey,
    solana_svm_feature_set::SVMFeatureSet,
    std::{collections::HashMap, sync::LazyLock},
};

const fn feature_u64(feature: &Pubkey) -> u64 {
    let feature_id = feature.to_bytes();
    feature_id[0] as u64
        | (feature_id[1] as u64) << 8
        | (feature_id[2] as u64) << 16
        | (feature_id[3] as u64) << 24
        | (feature_id[4] as u64) << 32
        | (feature_id[5] as u64) << 40
        | (feature_id[6] as u64) << 48
        | (feature_id[7] as u64) << 56
}

static INDEXED_FEATURES: LazyLock<HashMap<u64, Pubkey>> = LazyLock::new(|| {
    FEATURE_NAMES
        .keys()
        .map(|pubkey| (feature_u64(pubkey), *pubkey))
        .collect()
});

/// Build a `FeatureSet` from a protobuf feature set.
pub fn feature_set_from_proto(value: &ProtoFeatureSet) -> FeatureSet {
    let mut active = AHashMap::default();
    let mut inactive: AHashSet<Pubkey> = FEATURE_NAMES.keys().cloned().collect();
    for id in &value.features {
        if let Some(pubkey) = INDEXED_FEATURES.get(id) {
            inactive.remove(pubkey);
            active.insert(*pubkey, 0);
        }
    }
    FeatureSet::new(active, inactive)
}

/// Build a protobuf feature set from an `SVMFeatureSet`.
pub fn feature_set_to_proto(feature_set: &SVMFeatureSet) -> ProtoFeatureSet {
    let mut features = FeatureSet::from_runtime_features(feature_set)
        .active()
        .keys()
        .map(feature_u64)
        .collect::<Vec<_>>();
    features.sort_unstable();
    ProtoFeatureSet { features }
}
