use {
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
};

#[cfg_attr(feature = "frozen-abi", derive(AbiExample, AbiEnumVisitor))]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
enum CompressionType {
    #[default]
    Uncompressed,
    GZip,
    BZip2,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EpochIncompleteSlots {
    first: Slot,
    compression: CompressionType,
    #[serde(with = "serde_bytes")]
    compressed_list: Vec<u8>,
}
