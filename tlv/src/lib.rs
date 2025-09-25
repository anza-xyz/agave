use {
    bincode::Options,
    bytes::{Buf, Bytes, BytesMut},
    serde::{Deserialize, Serialize},
};

mod tlv_record;
pub use tlv_record::{tlv_bincode_options, TlvDecodeError, TlvEncodeError, TlvRecord};

const MAX_VALUE_LENGTH: usize = 1500;

/// Macro that provides a quick and easy way to define TLV compatible enums.
///
/// ```
/// use solana_tlv::define_tlv_enum;
/// use bytes::Bytes;
///
/// define_tlv_enum! (pub(crate) enum Extension {
///    1=>Thing(u64), // this will use bincode
///    3=>DoGood(()), // this will store the tag and no data
///    4=>Mac([u8; 16]), // some kind of signature for packet
///    #[raw]
///    5=>ByteArray(Bytes), // this will get bytes included verbatim
/// });
/// ```
/// The `#[raw]` attribute is only supported for enum variants holding Bytes instances.
#[macro_export]
macro_rules! define_tlv_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $enum_name:ident {
            $(
                $(#[$attr:tt])*   // optional per-variant attribute #[raw]
                $tag:literal => $variant:ident($inner:ty)
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

        impl $crate::TlvSerialize for $enum_name {
            /// Serialize enum into bytes by first converting into TlvRecord,
            /// and then serializing resulting records into provided byte buffer
            fn serialize(&self, buffer: &mut bytes::BytesMut) -> Result<(), $crate::TlvEncodeError> {
                let tlv_rec = $crate::TlvRecord::try_from(self)?;
                tlv_rec.serialize(buffer)
            }
        }

        // Serialization: TryFrom<&Enum> for TlvRecord
        impl TryFrom<&$enum_name> for $crate::TlvRecord {
            type Error = $crate::TlvEncodeError;
            fn try_from(value: &$enum_name) -> Result<Self, Self::Error> {
                match value {
                       $(
                           $enum_name::$variant(inner) => define_tlv_enum!(@serialize [ $(#[$attr])* ] inner, $tag),
                       )*
                }
            }
        }

        // Deserialization: TryFrom<&TlvRecord> for enum variants
        impl TryFrom<&$crate::TlvRecord> for $enum_name {
            type Error = $crate::TlvDecodeError;
            fn try_from(record: &$crate::TlvRecord) -> Result<Self, Self::Error> {

                match record.tag() {
                    $(
                        $tag => define_tlv_enum!(@decode [ $(#[$attr])* ] record $enum_name::$variant, $inner),
                    )*
                    _ => Err($crate::TlvDecodeError::InvalidTag(record.tag())),
                }
            }
        }

        // Same but for owned TlvRecord
        impl TryFrom< $crate::TlvRecord> for $enum_name {
            type Error = $crate::TlvDecodeError;
            fn try_from(record: $crate::TlvRecord) -> Result<Self, Self::Error> {
                Self::try_from(&record)
            }
        }
    };

    // Helper to use raw value for #[raw] variants
    (@decode [#[$($raw:tt)*]] $record:ident $variant:path, $inner:ty) => {
        Ok($variant(<$inner>::from($record.value().to_owned())))
    };

    // Helper to deserialize with bincode
    (@decode [] $record:ident $variant:path, $inner:ty) => {
        Ok($variant($crate::deserialize_tagged_value_with_bincode($record)?))
    };

    // Helper to be able to serialize #[raw] variants
    (@serialize [#[raw]] $inner:ident, $tag:expr) => {
        $crate::TlvRecord::new (
             $tag,
             $inner.to_owned(),
        )
    };

    // Helper to serialize via bincode
    (@serialize [] $inner:ident, $tag:expr) => {
        $crate::serialize_tagged_value_with_bincode($inner, $tag)
    };
}

/// To be used by define_tlv_enum! macro, do not use directly
pub fn deserialize_tagged_value_with_bincode<'a, INNER: Deserialize<'a>>(
    record: &'a TlvRecord,
) -> Result<INNER, TlvDecodeError> {
    let opts = tlv_bincode_options();
    Ok(opts.deserialize::<INNER>(record.value())?)
}

/// To be used by define_tlv_enum! macro, do not use directly
pub fn serialize_tagged_value_with_bincode<T: Serialize>(
    inner: &T,
    tag: u8,
) -> Result<TlvRecord, TlvEncodeError> {
    let opts = tlv_bincode_options();
    let data = opts.serialize(inner)?;
    TlvRecord::new(tag, Bytes::from_owner(data))
}

/// Marks types that can be serialized using TLV.
/// Implemented by the define_tlv_enum macro.
/// Do not implement this trait for your types.
pub trait TlvSerialize {
    fn serialize(&self, buffer: &mut BytesMut) -> Result<(), TlvEncodeError>;
}

/// Walks TLV records in a buffer without parsing them.
///
/// This iterator is not guaranteed to walk the entirety of the provided
/// buffer, and will instead stop on the first invalid entry.
pub struct TlvIter {
    entries: Bytes,
}

impl TlvIter {
    /// Construct an iterator over Bytes object holding serialized TlvRecords
    ///
    /// This iterator is not guaranteed to walk the entirety of the provided
    /// buffer, and will instead stop on the first invalid entry.
    pub fn new(entries: Bytes) -> Self {
        Self { entries }
    }
}

impl Iterator for TlvIter {
    type Item = (usize, TlvRecord);
    /// Consume next item from the iterator.
    /// If this returns None, no more valid items can be read from the buffer.
    fn next(&mut self) -> Option<Self::Item> {
        let total_bytes = self.entries.remaining();
        let item = TlvRecord::deserialize(&mut self.entries)?;
        let bytes_consumed = total_bytes.saturating_sub(self.entries.remaining());
        Some((bytes_consumed, item))
    }
}

/// Serialize all entries into given buffer.
/// Buffer must have preallocated memory.
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
    TlvIter::new(buffer).filter_map(|(_, v)| v.try_into().ok())
}

#[cfg(test)]
mod tests {
    use {
        crate::{define_tlv_enum, deserialize_from_buffer, serialize_into_buffer},
        bytes::{Bytes, BytesMut},
        serde::Serialize,
        solana_short_vec::decode_shortu16_len,
    };

    define_tlv_enum! (pub(crate) enum ExtensionNew {
        1=>Test(u64),
        2=>LegacyString(String),
        3=>NewString(String),
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

    /// Test that TLV encoded data is forwards-compatible,
    /// i.e. that legacy TLV data can be decoded by a new
    /// receiver
    #[test]
    fn test_tlv_forward_compat() {
        let legacy_tlv_data = [
            ExtensionLegacy::Test(42),
            ExtensionLegacy::LegacyString(String::from("foo")),
        ];
        let mut buffer = BytesMut::with_capacity(2000);
        serialize_into_buffer(&legacy_tlv_data, &mut buffer).unwrap();

        let buffer = buffer.freeze();
        // Parse the same bytes using new parser
        let new_recovered: Vec<ExtensionNew> = deserialize_from_buffer(buffer.clone()).collect();
        assert!(matches!(new_recovered[0], ExtensionNew::Test(42)));
        if let ExtensionNew::LegacyString(s) = &new_recovered[1] {
            assert_eq!(s, "foo");
        } else {
            panic!("Wrong deserialization")
        };
    }

    #[test]
    fn test_tlv_wire_format() {
        let tlv_data = vec![
            ExtensionNew::Test(u64::MAX),
            ExtensionNew::ByteArray(Bytes::from(vec![77u8; 256])),
        ];
        let mut buffer = BytesMut::with_capacity(2000);
        serialize_into_buffer(&tlv_data, &mut buffer).unwrap();
        let field_1 = &buffer[0..10];
        assert_eq!(field_1[0], 1, "tag of first field should be 1");
        assert_eq!(field_1[1], 8, "length of first field should be 8");
        assert_eq!(
            field_1[2..10],
            u64::MAX.to_le_bytes(),
            "Value of first field should be u64::MAX"
        );
        let field_2 = &buffer[10..];
        assert_eq!(field_2[0], 5, "tag of second field should be 5");
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

    // checks that we are wire-compatible with gossip TLV impl
    #[test]
    fn test_abi_wire_compat_gossip() {
        use solana_short_vec as short_vec;
        #[derive(Serialize)]
        struct TlvRecord {
            typ: u8, // type
            #[serde(with = "short_vec")]
            bytes: Vec<u8>, // length and value
        }
        let tlv_data = vec![ExtensionNew::Test(u64::MAX)];
        let mut buffer = BytesMut::with_capacity(50);
        serialize_into_buffer(&tlv_data, &mut buffer).unwrap();
        let rec = TlvRecord {
            typ: 1,
            bytes: vec![255, 255, 255, 255, 255, 255, 255, 255],
        };
        let bincode_vec = bincode::serialize(&rec).unwrap();
        assert_eq!(bincode_vec, buffer.as_ref());
    }
}
