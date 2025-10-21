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

/// Type-Length-Value encoded entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlvRecord {
    // tag (aka type)
    tag: u8,
    // virtual: length: ShortU16 of the following byte slice
    // will be encoded as ShortU16 on the wire

    // serialized bytes of the value
    value: Bytes,
}

impl TlvRecord {
    /// Construct a new TlvRecord
    pub fn new(tag: u8, value: Bytes) -> Result<Self, TlvEncodeError> {
        if tag == 0 {
            return Err(TlvEncodeError::InvalidTag(tag));
        }
        if value.len() > MAX_VALUE_LENGTH {
            return Err(TlvEncodeError::PayloadTooBig);
        }
        Ok(Self { tag, value })
    }

    pub fn tag(&self) -> u8 {
        self.tag
    }

    pub fn value(&self) -> &Bytes {
        &self.value
    }

    /// Fetches a TlvRecord from a given Bytes slice and advances buffer pointers appropriately.
    /// If this returns None, the record was not properly formatted,
    /// and one must assume the rest of the buffer is not readable.
    /// This function is zero-copy.
    pub fn deserialize(buffer: &mut Bytes) -> Option<Self> {
        let tag = buffer.try_get_u8().ok()?;
        let (length, consumed) = short_vec::decode_shortu16_len(buffer).ok()?;
        buffer.advance(consumed);
        if length > MAX_VALUE_LENGTH.min(buffer.len()) {
            return None;
        }
        Some(Self {
            tag,
            value: buffer.split_to(length),
        })
    }
}

/// Helper to allow serialization into BytesMut.
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
        // check for sanity
        let opts = tlv_bincode_options();
        let len = ShortU16(self.value.len() as u16);
        #[allow(clippy::arithmetic_side_effects)] // this has no chance to overflow u64
        let total_len = 1 + opts.serialized_size(&len)? + self.value.len() as u64;
        if total_len > buffer.spare_capacity_mut().len() as u64 {
            return Err(TlvEncodeError::NotEnoughSpace);
        }
        // write tag
        buffer.put_u8(self.tag);
        // write length
        let writer = BytesMutWriter(buffer);
        let opts = tlv_bincode_options();
        opts.serialize_into(writer, &len)?;
        // write value
        buffer.put_slice(&self.value);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlvDecodeError {
    #[error("Invalid tag: {0}")]
    InvalidTag(u8),
    #[error("Malformed payload: {0}")]
    MalformedPayload(#[from] bincode::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum TlvEncodeError {
    #[error("Invalid tag: {0}")]
    InvalidTag(u8),
    #[error("Not enough space for payload in the buffer")]
    NotEnoughSpace,
    #[error("Payload exceeds MAX_VALUE_LENGTH")]
    PayloadTooBig,
    #[error("Malformed payload: {0}")]
    MalformedPayload(#[from] bincode::Error),
}

#[cfg(test)]
mod tests {
    use {
        crate::{TlvEncodeError, TlvRecord, TlvSerialize},
        bytes::{Bytes, BytesMut},
    };

    #[test]
    fn test_serialize() {
        let rec = TlvRecord {
            tag: 1,
            value: Bytes::from_static(b"blabla"),
        };
        let mut buffer = BytesMut::with_capacity(4);
        assert!(matches!(
            rec.serialize(&mut buffer),
            Err(TlvEncodeError::NotEnoughSpace)
        ));
        let mut buffer = BytesMut::with_capacity(16);
        rec.serialize(&mut buffer).unwrap();
        assert_eq!(buffer[0], 1);
        assert_eq!(buffer[1], 6);

        assert_eq!(buffer[2..8], *b"blabla");
    }
}
