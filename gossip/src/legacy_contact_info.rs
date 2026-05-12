use {crate::crds_data::reject_deserialize, serde::Serialize};

/// Structure representing a node on the network
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyContactInfo {}
reject_deserialize!(LegacyContactInfo, "LegacyContactInfo is deprecated");
