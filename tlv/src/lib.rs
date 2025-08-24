use bytes::{Bytes, BytesMut};

pub mod signature;
mod tlv_record;
pub use tlv_record::{tlv_bincode_options, TlvDecodeError, TlvEncodeError, TlvRecord};

const MAX_VALUE_LENGTH: usize = 1500;

/// Macro that provides a quick and easy way to define TLV compatible enums
///
/// ```
/// use solana_tlv::{define_tlv_enum, signature::Signature};
/// use bytes::Bytes;
///
/// define_tlv_enum! (pub(crate) enum Extension {
///    1=>Thing(u64), // this will use bincode
///    3=>DoGood(()), // this will store the tag and no data
///    4=>Mac(Signature<16>), // and this allows to sign packets
///    #[raw]
///    5=>ByteArray(Bytes), // this will get bytes included verbatim
/// });
/// ```
#[macro_export]
macro_rules! define_tlv_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $enum_name:ident {
            $(
                $(#[$attr:tt])*   // optional per-variant attributes
                $typ:literal => $variant:ident($inner:ty)
            ),* $(,)?
        }
    ) => {
        // Enum definition
        $(#[$meta])*
        #[derive(Debug, Clone, Eq, PartialEq)]
        $vis enum $enum_name {
            $(
                $variant($inner),
            )*
        }

        // Serialize enum by converting into TLV record
        impl $crate::TlvSerialize for $enum_name {
            fn serialize(&self, buffer: &mut bytes::BytesMut) -> Result<(), $crate::TlvEncodeError> {
                let tlv_rec = $crate::TlvRecord::try_from(self)
                    .map_err($crate::TlvEncodeError::MalformedPayload)?;
                tlv_rec.serialize(buffer)
            }
        }

        // TryFrom<TlvRecord> for owned record
        impl TryFrom< $crate::TlvRecord> for $enum_name {
            type Error = $crate::TlvDecodeError;
            fn try_from(record: $crate::TlvRecord) -> Result<Self, Self::Error> {
                Self::try_from(&record)
            }
        }

        // TryFrom<&Enum> for TLV record
        impl TryFrom<&$enum_name> for $crate::TlvRecord {
            type Error = bincode::Error;
            fn try_from(value: &$enum_name) -> Result<Self, Self::Error> {
                match value {
                       $(
                           $enum_name::$variant(inner) => define_tlv_enum!(@serialize [ $(#[$attr])* ] inner, $typ),
                       )*
                }
            }
        }

        // TryFrom<&TlvRecord> for enum, with per-variant #[no_bincode] handling
        impl TryFrom<&$crate::TlvRecord> for $enum_name {
            type Error = $crate::TlvDecodeError;
            fn try_from(record: &$crate::TlvRecord) -> Result<Self, Self::Error> {
                use bincode::Options;
                let opts = $crate::tlv_bincode_options();

                match record.typ {
                    $(
                        $typ => define_tlv_enum!(@decode [ $(#[$attr])* ] record opts $enum_name::$variant, $inner),
                    )*
                    _ => Err($crate::TlvDecodeError::UnknownType(record.typ)),
                }
            }
        }
    };

    // Helper: use raw value for #[no_bincode] variants
    (@decode [#[$($raw:tt)*]] $record:ident $opts:ident $variant:path, $inner:ty) => {
        Ok($variant(<$inner>::from($record.value.clone())))
    };

    // Default: use bincode
    (@decode [] $record:ident $opts:ident $variant:path, $inner:ty) => {
        Ok($variant($opts.deserialize::<$inner>(&$record.value)?))
    };

    (@serialize [#[raw]] $inner:ident, $typ:expr) => {
        Ok($crate::TlvRecord {
            typ: $typ,
            value: bytes::Bytes::from($inner.to_owned()),
        })
    };

    (@serialize [] $inner:ident, $typ:expr) => {
        {use { bincode::Options};
        let opts = $crate::tlv_bincode_options();
        let data = opts.serialize($inner)?;
        Ok($crate::TlvRecord { typ: $typ, value: Bytes::from(data) })}
    };
}

/// This trait should be implemented by define_tlv_enum macro.
/// Do not implement this manually for your types.
pub trait TlvSerialize {
    fn serialize(&self, buffer: &mut BytesMut) -> Result<(), TlvEncodeError>;
}

/// Walk TLV records in a buffer without parsing them
pub struct TlvIter {
    entries: Bytes,
}

impl TlvIter {
    pub fn new(entries: Bytes) -> Self {
        Self { entries }
    }
}

impl Iterator for TlvIter {
    type Item = TlvRecord;
    fn next(&mut self) -> Option<Self::Item> {
        TlvRecord::deserialize(&mut self.entries)
    }
}

/// Serialize all entries into given buffer
/// Buffer must have preallocated memory
pub fn serialize_into_buffer<'a, T: 'a + TlvSerialize>(
    entries: &'a [T],
    buffer: &mut BytesMut,
) -> Result<(), TlvEncodeError> {
    for entry in entries {
        entry.serialize(buffer)?
    }
    Ok(())
}

/// Walk over a given buffer returning deserialized TLV items
/// This will quietly skip all invalid entries.
pub fn deserialize_from_buffer<T: TryFrom<TlvRecord>>(buffer: Bytes) -> impl Iterator<Item = T> {
    TlvIter::new(buffer).filter_map(|v| v.try_into().ok())
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            define_tlv_enum, deserialize_from_buffer, serialize_into_buffer, signature::Signature,
        },
        bytes::{Bytes, BytesMut},
        solana_short_vec::decode_shortu16_len,
    };

    define_tlv_enum! (pub(crate) enum ExtensionNew {
        1=>Test(u64),
        2=>LegacyString(String),
        3=>NewString(String),
        4=>Mac(Signature<16>),
        #[raw]
        5=>ByteArray(Bytes),
        6=>NewEmptyTag(()),
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
        let new_tlv_data = [
            ExtensionNew::Test(42),
            ExtensionNew::NewString(String::from("bla")),
            ExtensionNew::NewEmptyTag(()),
        ];
        let mut buffer = BytesMut::with_capacity(2000);
        serialize_into_buffer(&new_tlv_data, &mut buffer).unwrap();

        let buffer = buffer.freeze();
        // check that both TLV are encoded correctly
        let new_recovered: Vec<ExtensionNew> = deserialize_from_buffer(buffer.clone()).collect();
        assert!(matches!(new_recovered[0], ExtensionNew::Test(42)));
        if let ExtensionNew::NewString(s) = &new_recovered[1] {
            assert_eq!(s, "bla");
        } else {
            panic!("Wrong deserialization")
        };

        // Make sure legacy recover works correctly
        let legacy_recovered: Vec<ExtensionLegacy> =
            deserialize_from_buffer(buffer.clone()).collect();
        assert!(matches!(legacy_recovered[0], ExtensionLegacy::Test(42)));
        assert_eq!(
            legacy_recovered.len(),
            1,
            "Legacy parser should only recover  1 entry"
        )
    }
    /*
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
        let legacy: Vec<ExtensionLegacy> = crate::parse(&tlv_vec);
        assert!(matches!(legacy[0], ExtensionLegacy::Test(42)));
        if let ExtensionLegacy::LegacyString(s) = &legacy[1] {
            assert_eq!(s, "foo");
        } else {
            panic!("Wrong deserialization")
        };
        // Parse the same bytes using new parser
        let new: Vec<ExtensionNew> = crate::parse(&tlv_vec);
        assert!(matches!(new[0], ExtensionNew::Test(42)));
        if let ExtensionNew::LegacyString(s) = &new[1] {
            assert_eq!(s, "foo");
        } else {
            panic!("Wrong deserialization")
        };
    }*/

    #[test]
    fn test_tlv_wire_format() {
        let tlv_data = vec![
            ExtensionNew::Test(u64::MAX),
            ExtensionNew::ByteArray(Bytes::from(vec![77u8; 256])),
        ];
        let mut buffer = BytesMut::with_capacity(2000);
        serialize_into_buffer(&tlv_data, &mut buffer).unwrap();
        let field_1 = &buffer[0..10];
        assert_eq!(field_1[0], 1, "type of first field should be 1");
        assert_eq!(field_1[1], 8, "length of first field should be 8");
        assert_eq!(
            field_1[2..10],
            u64::MAX.to_le_bytes(),
            "Value of first field should be u64::MAX"
        );
        let field_2 = &buffer[10..];
        assert_eq!(field_2[0], 5, "type of second field should be 5");
        assert_eq!(
            decode_shortu16_len(&field_2[1..3]).unwrap(),
            (256, 2),
            "length of second field should be 256"
        );
        assert_eq!(field_2[3], 77, "Value of second field should be 77");
        assert_eq!(field_2[256 + 2], 77, "Value of second field should be 77");

        let recovered_data: Vec<ExtensionNew> = deserialize_from_buffer(buffer.freeze()).collect();
        assert_eq!(recovered_data, tlv_data)
    }
}
