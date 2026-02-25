use crate::lv_record::LvPayload;

use {
    bytes::{Bytes, BytesMut},
    wincode::{SchemaRead, SchemaWrite},
};

mod lv_record;
mod short_u16;
pub use lv_record::{TlvDecodeError, TlvEncodeError, TlvRecord};
use wincode::io::Cursor;

pub use lv_record::LvRecord;
pub use lv_record::TlvSchemaReader;

/// Walks TLV records in a buffer without parsing them.
///
/// This iterator is not guaranteed to walk the entirety of the provided
/// buffer, and will instead stop on the first invalid entry.
pub struct TlvIter<T>
where
    T: AsRef<[u8]>,
{
    entries: Cursor<T>,
}

impl<T> TlvIter<T>
where
    T: AsRef<[u8]>,
{
    /// Construct an iterator over Bytes object holding serialized TlvRecords
    ///
    /// This iterator is not guaranteed to walk the entirety of the provided
    /// buffer, and will instead stop on the first invalid entry.
    pub fn new(entries: T) -> Self {
        Self {
            entries: Cursor::new(entries),
        }
    }
}

impl<T> Iterator for TlvIter<T>
where
    T: AsRef<[u8]>,
{
    type Item = LvRecord<impl LvPayload>;
    /// Consume next item from the iterator.
    /// If this returns None, no more valid items can be read from the buffer.
    fn next(&mut self) -> Option<Self::Item> {
        let res: Result<Option<Self::Item>, _> = TlvSchemaReader::get(&mut self.entries);
        res.ok().flatten()
    }
}

/// Serialize all entries into given buffer.
/// Buffer must have preallocated memory.
pub fn serialize_into_buffer<'a, T: 'a + SchemaWrite<Src = T>, B>(
    entries: &'a [T],
    buffer: &mut Cursor<B>,
) -> Result<(), TlvEncodeError> {
    for entry in entries {
        wincode::serialize_into(buffer, &entry)?;
    }
    Ok(())
}

/// Walk over a given buffer returning deserialized TLV items
/// This will quietly skip all invalid entries.
pub fn deserialize_from_buffer<T: TryFrom<impl LvPayload>>(
    buffer: Bytes,
) -> impl Iterator<Item = T> {
    TlvIter::new(buffer).filter_map(|v| v.try_into().ok())
}

#[cfg(test)]
mod tests {
    use {
        crate::{deserialize_from_buffer, serialize_into_buffer, LvRecord},
        bytes::BytesMut,
        serde::Serialize,
        solana_short_vec::decode_shortu16_len,
        wincode::io::Cursor,
    };
    #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite, PartialEq, Eq)]
    enum ExtensionNew {
        #[wincode(tag = 1)]
        Test(LvRecord<u64>),
        #[wincode(tag = 2)]
        LegacyString(LvRecord<String>),
        #[wincode(tag = 3)]
        NewString(LvRecord<String>),
        #[wincode(tag = 6)]
        NewEmptyTag(()),
    }

    #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite, PartialEq, Eq)]
    enum ExtensionLegacy {
        #[wincode(tag = 1)]
        Test(LvRecord<u64>),
        #[wincode(tag = 2)]
        LegacyString(LvRecord<String>),
    }

    /// Test that TLV encoded data is backwards-compatible,
    /// i.e. that new TLV data can be decoded by a new
    /// receiver where possible, and skipped otherwise
    #[test]
    fn test_tlv_backwards_compat() {
        let new_tlv_data = [
            ExtensionNew::Test(LvRecord::new(42)),
            ExtensionNew::NewString(LvRecord::new(String::from("bla"))),
            ExtensionNew::NewEmptyTag(()),
        ];
        let mut buffer = BytesMut::with_capacity(2000);
        serialize_into_buffer(&new_tlv_data, &mut buffer).unwrap();

        let buffer = buffer.freeze();
        // check that both TLV are encoded correctly
        let new_recovered: Vec<ExtensionNew> = deserialize_from_buffer(buffer.clone()).collect();
        assert_eq!(new_recovered[0], ExtensionNew::Test(LvRecord::new(42)));
        if let ExtensionNew::NewString(s) = &new_recovered[1] {
            assert_eq!(s.payload(), "bla");
        } else {
            panic!("Wrong deserialization")
        };

        // Make sure legacy recover works correctly
        let legacy_recovered: Vec<ExtensionLegacy> =
            deserialize_from_buffer(buffer.clone()).collect();
        assert_eq!(
            legacy_recovered[0],
            ExtensionLegacy::Test(LvRecord::new(42))
        );
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
            ExtensionLegacy::Test(LvRecord::new(42)),
            ExtensionLegacy::LegacyString(LvRecord::new(String::from("foo"))),
        ];
        let mut buffer = Vec::with_capacity(2000);
        serialize_into_buffer(&legacy_tlv_data, &mut Cursor(buffer)).unwrap();

        // Parse the same bytes using new parser
        let new_recovered: Vec<ExtensionNew> = deserialize_from_buffer(buffer.clone()).collect();
        assert_eq!(new_recovered[0], ExtensionNew::Test(LvRecord::new(42)));
        if let ExtensionNew::LegacyString(s) = &new_recovered[1] {
            assert_eq!(s.payload(), "foo");
        } else {
            panic!("Wrong deserialization")
        };
    }

    // #[test]
    // fn test_tlv_wire_format() {
    //     let tlv_data = vec![
    //         ExtensionNew::Test(LvRecord(u64::MAX)),
    //         ExtensionNew::ByteArray(vec![77u8; 256].into_boxed_slice()),
    //     ];
    //     let mut buffer = BytesMut::with_capacity(2000);
    //     serialize_into_buffer(&tlv_data, &mut buffer).unwrap();
    //     let field_1 = &buffer[0..10];
    //     assert_eq!(field_1[0], 1, "tag of first field should be 1");
    //     assert_eq!(field_1[1], 8, "length of first field should be 8");
    //     assert_eq!(
    //         field_1[2..10],
    //         u64::MAX.to_le_bytes(),
    //         "Value of first field should be u64::MAX"
    //     );
    //     let field_2 = &buffer[10..];
    //     assert_eq!(field_2[0], 5, "tag of second field should be 5");
    //     assert_eq!(
    //         decode_shortu16_len(&field_2[1..3]).unwrap(),
    //         (256, 2),
    //         "length of second field should be 256"
    //     );
    //     assert_eq!(field_2[3], 77, "Value of second field should be 77");
    //     assert_eq!(field_2[256 + 2], 77, "Value of second field should be 77");

    //     let recovered_data: Vec<ExtensionNew> = deserialize_from_buffer(buffer.freeze()).collect();
    //     assert_eq!(recovered_data, tlv_data)
    // }

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
        let tlv_data = vec![ExtensionNew::Test(LvRecord::new(u64::MAX))];
        let mut buffer: Vec<u8> = Vec::with_capacity(50);
        serialize_into_buffer(&tlv_data, &mut Cursor::new(&mut buffer)).unwrap();
        let rec = TlvRecord {
            typ: 1,
            bytes: vec![255, 255, 255, 255, 255, 255, 255, 255],
        };
        let bincode_vec = bincode::serialize(&rec).unwrap();
        assert_eq!(&bincode_vec, &buffer);
    }
}
