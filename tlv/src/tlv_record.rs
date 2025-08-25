use {
    crate::{TlvSerialize, MAX_VALUE_LENGTH},
    bincode::Options,
    bytes::{Buf, BufMut, Bytes, BytesMut},
    solana_short_vec::{self as short_vec, ShortU16},
    std::io::{self, Write},
};

#[inline(always)]
pub fn tlv_bincode_options() -> impl Options {
    bincode::DefaultOptions::new()
        .with_limit(MAX_VALUE_LENGTH as u64)
        .with_fixint_encoding()
}

/// Type-Length-Value encoded entry
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlvRecord {
    // type
    pub typ: u8,
    // virtual: length: ShortU16 of the following byte slice
    // will be encoded as ShortU16 on the wire
    // serialized bytes of the value
    pub value: Bytes,
}

impl TlvRecord {
    /// Fetches a TlvRecord from a given Bytes slice and advances buffer pointers appropriately.
    /// If this returns None, the record was not properly formatted,
    /// one must assume the rest of the buffer is not readable.
    /// This function is zero-copy.
    pub fn deserialize(buffer: &mut Bytes) -> Option<Self> {
        let typ = buffer.try_get_u8().ok()?;
        let (length, consumed) = short_vec::decode_shortu16_len(buffer).ok()?;
        if length > MAX_VALUE_LENGTH {
            return None;
        }
        buffer.advance(consumed);
        if length > buffer.len() {
            return None;
        }
        Some(Self {
            typ,
            value: buffer.split_to(length),
        })
    }
}

/// Helper to allow serialization into BytesMut
struct BytesMutWriter<'a>(&'a mut BytesMut);

impl Write for BytesMutWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TlvSerialize for TlvRecord {
    /// Write the TlvRecord instance into provided byte buffer.
    fn serialize(&self, buffer: &mut BytesMut) -> Result<(), TlvEncodeError> {
        if self.value.len() > MAX_VALUE_LENGTH {
            return Err(TlvEncodeError::MalformedTlv);
        }
        let len = ShortU16(self.value.len() as u16);

        buffer.put_u8(self.typ);
        let opts = tlv_bincode_options();
        let writer = BytesMutWriter(buffer);
        opts.serialize_into(writer, &len)?;
        if buffer.spare_capacity_mut().len() < self.value.len() {
            return Err(TlvEncodeError::NotEnoughSpace);
        }
        buffer.put_slice(&self.value);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlvDecodeError {
    #[error("Unknown type: {0}")]
    UnknownType(u8),
    #[error("Malformed payload: {0}")]
    MalformedPayload(#[from] bincode::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum TlvEncodeError {
    #[error("Not enough space for payload")]
    NotEnoughSpace,
    #[error("Malformed TLV record")]
    MalformedTlv,
    #[error("Malformed payload: {0}")]
    MalformedPayload(#[from] bincode::Error),
}
