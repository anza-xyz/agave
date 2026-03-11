use {
    serde::{Deserialize, Serialize},
    solana_short_vec as short_vec,
};

/// Type-Length-Value encoding wrapper for bincode
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct TlvRecord {
    // type
    pub(crate) typ: u8,
    // length and serialized bytes of the value
    #[serde(with = "short_vec")]
    pub(crate) bytes: Vec<u8>,
}

/// Macro that provides a quick and easy way to define TLV compatible enums
#[macro_export]
macro_rules! define_tlv_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $enum_name:ident {
            $($typ:literal => $variant:ident($inner:ty)),* $(,)?
        }
    ) => {
        // add the doc-comment if present
        $(#[$meta])*
        // define the enum itself
        #[derive(Debug, Clone, Eq, PartialEq)]
        $vis enum $enum_name {
            $(
                $variant($inner),
            )*
        }

        // Serialize enum by first converting into TlvRecord, and then serializing that
        impl serde::Serialize for $enum_name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let tlv_rec = TlvRecord::try_from(self).map_err(|e| serde::ser::Error::custom(e))?;
                tlv_rec.serialize(serializer)
            }
        }

        // define conversion from TLV wire format
        impl TryFrom<&TlvRecord> for $enum_name {
            type Error = TlvDecodeError;
            fn try_from(value: &TlvRecord) -> Result<Self, Self::Error> {
                match value.typ {
                    $(
                        $typ => Ok(Self::$variant(bincode::deserialize::<$inner>(&value.bytes)?)),
                    )*
                    _ => Err(TlvDecodeError::UnknownType(value.typ)),
                }
            }
        }
        // define conversion into TLV wire format
        impl TryFrom<&$enum_name> for TlvRecord {
            type Error = bincode::Error;
            fn try_from(value: &$enum_name) -> Result<Self, Self::Error> {
                use serde::ser::Error;
                match value {
                    $(
                        $enum_name::$variant(inner) => Ok(TlvRecord {
                            typ: $typ,
                            bytes: bincode::serialize(inner)?,
                        }),
                    )*
                    #[allow(unreachable_patterns)]
                    _ => Err(bincode::Error::custom("Unsupported enum variant")),
                }
            }
        }
    };
}

#[derive(Debug, thiserror::Error)]
pub enum TlvDecodeError {
    #[error("Unknown type: {0}")]
    UnknownType(u8),
    #[error("Malformed payload: {0}")]
    MalformedPayload(#[from] bincode::Error),
}

/// Parses a slice of serialized TLV records into a provided type. Unsupported
/// TLV records are ignored.
pub(crate) fn parse<'a, T: TryFrom<&'a TlvRecord>>(entries: &'a [TlvRecord]) -> Vec<T> {
    entries.iter().filter_map(|v| T::try_from(v).ok()).collect()
}

#[cfg(test)]
mod tests {
    use crate::tlv::{TlvDecodeError, TlvRecord};

    define_tlv_enum! (pub(crate) enum ExtensionNew {
        1=>Test(u64),
        2=>LegacyString(String),
        3=>NewString(String),
    });

    define_tlv_enum! ( pub(crate) enum ExtensionLegacy {
        1=>Test(u64),
        2=>LegacyString(String),
    });

    /// Test that TLV encoded data is backwards-compatible,
    /// i.e. that new TLV data can be decoded by a new
    /// receiver where possible, and skipped otherwise
    #[test]
    fn test_tlv_backwards_compat() {
        let new_tlv_data = vec![
            ExtensionNew::Test(42),
            ExtensionNew::NewString(String::from("bla")),
        ];

        let new_bytes = bincode::serialize(&new_tlv_data).unwrap();
        let tlv_vec: Vec<TlvRecord> = bincode::deserialize(&new_bytes).unwrap();
        // check that both TLV are encoded correctly
        let new: Vec<ExtensionNew> = crate::tlv::parse(&tlv_vec);
        assert!(matches!(new[0], ExtensionNew::Test(42)));
        if let ExtensionNew::NewString(s) = &new[1] {
            assert_eq!(s, "bla");
        } else {
            panic!("Wrong deserialization")
        };
        // Make sure legacy recover works correctly
        let legacy: Vec<ExtensionLegacy> = crate::tlv::parse(&tlv_vec);
        assert!(matches!(legacy[0], ExtensionLegacy::Test(42)));
        assert_eq!(
            legacy.len(),
            1,
            "Legacy parser should only recover  1 entry"
        )
    }

    /// Test that TLV encoded data is forwards-compatible,
    /// i.e. that legacy TLV data can be decoded by a new
    /// receiver
    #[test]
    fn test_tlv_forward_compat() {
        let legacy_tlv_data = vec![
            ExtensionLegacy::Test(42),
            ExtensionLegacy::LegacyString(String::from("foo")),
        ];
        let legacy_bytes = bincode::serialize(&legacy_tlv_data).unwrap();

        let tlv_vec: Vec<TlvRecord> = bincode::deserialize(&legacy_bytes).unwrap();
        // Just in case make sure that legacy data is serialized correctly
        let legacy: Vec<ExtensionLegacy> = crate::tlv::parse(&tlv_vec);
        assert!(matches!(legacy[0], ExtensionLegacy::Test(42)));
        if let ExtensionLegacy::LegacyString(s) = &legacy[1] {
            assert_eq!(s, "foo");
        } else {
            panic!("Wrong deserialization")
        };
        // Parse the same bytes using new parser
        let new: Vec<ExtensionNew> = crate::tlv::parse(&tlv_vec);
        assert!(matches!(new[0], ExtensionNew::Test(42)));
        if let ExtensionNew::LegacyString(s) = &new[1] {
            assert_eq!(s, "foo");
        } else {
            panic!("Wrong deserialization")
        };
    }
}
