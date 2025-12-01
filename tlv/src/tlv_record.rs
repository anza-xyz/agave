use {
    crate::{MAX_VALUE_LENGTH, TlvSerialize},
    bytes::BytesMut,
    wincode::{SchemaRead, SchemaWrite, containers, io::Reader, len::ShortU16Len},
};

/// Type-Length-Value encoded entry.
#[derive(Clone, Debug, Eq, PartialEq, SchemaRead, SchemaWrite)]
pub struct TlvRecord {
    // tag (aka type)
    tag: u8,
    // virtual: length: ShortU16 of the following byte slice
    // serialized bytes of the value
    #[wincode(with = "containers::Box<[u8], ShortU16Len>")]
    value: Box<[u8]>,
}

impl TlvRecord {
    /// Construct a new TlvRecord
    pub fn new(tag: u8, value: Box<[u8]>) -> Result<Self, TlvEncodeError> {
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

    pub fn value(&self) -> &[u8] {
        &self.value
    }

    pub fn serialized_size(&self) -> usize {
        wincode::serialized_size(self).unwrap_or(0) as usize
    }

    /// Fetches a TlvRecord from a given Reader and advances buffer pointers appropriately.
    /// If this returns None, the record was not properly formatted,
    /// and one must assume the rest of the buffer is not readable.
    pub fn deserialize<'a>(buffer: &mut impl Reader<'a>) -> Option<Self> {
        Some(Self::get(buffer).ok()?)
    }
}

impl TlvSerialize for TlvRecord {
    /// Write the TlvRecord instance into provided byte buffer.
    fn serialize(&self, buffer: &mut BytesMut) -> Result<(), TlvEncodeError> {
        let serialized_len = self.serialized_size();
        if serialized_len > buffer.spare_capacity_mut().len() {
            return Err(TlvEncodeError::NotEnoughSpace);
        }
        let mut dst = buffer.spare_capacity_mut();
        let res = wincode::serialize_into(&mut dst, self);
        unsafe {
            buffer.set_len(
                buffer
                    .len()
                    .checked_add(serialized_len)
                    .ok_or(TlvEncodeError::PayloadTooBig)?,
            );
        }
        res.map_err(TlvEncodeError::from)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlvDecodeError {
    #[error("Invalid tag: {0}")]
    InvalidTag(u8),
    #[error("Malformed payload: {0}")]
    MalformedPayload(#[from] wincode::ReadError),
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
    MalformedPayload(#[from] wincode::WriteError),
}

#[cfg(test)]
mod tests {
    use {
        crate::{TlvEncodeError, TlvRecord, TlvSerialize},
        bytes::BytesMut,
        solana_short_vec::decode_shortu16_len,
        std::mem::{MaybeUninit, ptr, transmute},
        wincode::{
            ReadResult, SchemaRead, SchemaWrite, WriteError, WriteResult,
            error::{read_length_encoding_overflow, write_length_encoding_overflow},
            io::Reader,
        },
    };

    #[test]
    fn test_serialize() {
        let message = b"blabla";
        let rec = TlvRecord {
            tag: 1,
            value: Box::new(*message),
        };
        let mut buffer = BytesMut::with_capacity(4);
        assert!(matches!(
            rec.serialize(&mut buffer),
            Err(TlvEncodeError::NotEnoughSpace)
        ));
        let mut buffer = BytesMut::with_capacity(16);
        rec.serialize(&mut buffer).unwrap();
        assert_eq!(buffer[0], 1, "Tag should be 1");
        assert_eq!(
            buffer[1],
            message.len() as u8,
            "Length should match length of message"
        );

        assert_eq!(buffer[2..2 + message.len()], *message);
        assert_eq!(
            buffer.len(),
            message.len() + 2,
            "Buffer length should be set correctly"
        );
    }

    #[test]
    fn test_deserialize() {
        let message = b"blabla";
        let rec = TlvRecord {
            tag: 1,
            value: Box::new(*message),
        };
        let mut buffer = BytesMut::with_capacity(16);
        rec.serialize(&mut buffer).unwrap();
        {
            let mut corrupted = buffer.clone();
            corrupted[1] = 255; // corrupt the length field
            let mut cursor = wincode::io::Cursor::new(corrupted.freeze());
            assert!(TlvRecord::deserialize(&mut cursor).is_none());
        }
        {
            let mut cursor = wincode::io::Cursor::new(buffer.freeze());
            let received = TlvRecord::deserialize(&mut cursor).unwrap();
            assert_eq!(cursor.position(), rec.serialized_size());
            assert_eq!(received.value(), rec.value());
        }
    }

    #[derive(Debug)]
    #[repr(transparent)]
    pub struct ShortU16(u16);

    impl From<ShortU16> for usize {
        fn from(value: ShortU16) -> Self {
            value.0 as usize
        }
    }

    /// Branchless computation of the number of bytes needed to encode a short u16.
    ///
    /// See [`solana_short_vec::ShortU16`] for more details.
    #[inline(always)]
    #[allow(clippy::arithmetic_side_effects)]
    fn short_u16_bytes_needed(len: u16) -> usize {
        1 + (len >= 0x80) as usize + (len >= 0x4000) as usize
    }

    #[inline(always)]
    fn try_short_u16_bytes_needed<T: TryInto<u16>>(len: T) -> WriteResult<usize> {
        match len.try_into() {
            Ok(len) => Ok(short_u16_bytes_needed(len)),
            Err(_) => Err(write_length_encoding_overflow("u16::MAX")),
        }
    }

    /// Encode a short u16 into the given buffer.
    ///
    /// See [`solana_short_vec::ShortU16`] for more details.
    ///
    /// # Safety
    ///
    /// - `dst` must be a valid for writes.
    /// - `dst` must be valid for `needed` bytes.
    #[inline(always)]
    unsafe fn encode_short_u16(dst: *mut u8, needed: usize, len: u16) {
        // From `solana_short_vec`:
        //
        // u16 serialized with 1 to 3 bytes. If the value is above
        // 0x7f, the top bit is set and the remaining value is stored in the next
        // bytes. Each byte follows the same pattern until the 3rd byte. The 3rd
        // byte may only have the 2 least-significant bits set, otherwise the encoded
        // value will overflow the u16.
        match needed {
            1 => ptr::write(dst, len as u8),
            2 => {
                ptr::write(dst, ((len & 0x7f) as u8) | 0x80);
                ptr::write(dst.add(1), (len >> 7) as u8);
            }
            3 => {
                ptr::write(dst, ((len & 0x7f) as u8) | 0x80);
                ptr::write(dst.add(1), (((len >> 7) & 0x7f) as u8) | 0x80);
                ptr::write(dst.add(2), (len >> 14) as u8);
            }
            _ => unreachable!(),
        }
    }

    impl<'de> SchemaRead<'de> for ShortU16 {
        type Dst = ShortU16;

        fn read(
            reader: &mut impl Reader<'de>,
            dst: &mut std::mem::MaybeUninit<Self::Dst>,
        ) -> ReadResult<()> {
            let Ok((len, read)) = decode_shortu16_len(reader.fill_buf(3)?) else {
                return Err(read_length_encoding_overflow("u16::MAX"));
            };
            unsafe { reader.consume_unchecked(read) };
            Ok(len)
        }
    }

    impl SchemaWrite for ShortU16 {
        type Src = ShortU16;

        fn size_of(src: &Self::Src) -> wincode::WriteResult<usize> {
            try_short_u16_bytes_needed(len)
        }

        fn write(
            writer: &mut impl wincode::io::Writer,
            src: &Self::Src,
        ) -> wincode::WriteResult<()> {
            let src = src.0;
            let needed = short_u16_bytes_needed(src);
            let mut buf = [MaybeUninit::<u8>::uninit(); 3];
            // SAFETY: short_u16 uses a maximum of 3 bytes, so the buffer is always large enough.
            unsafe { encode_short_u16(buf.as_mut_ptr().cast(), needed, src) };
            // SAFETY: encode_short_u16 writes exactly `needed` bytes.
            let buf =
                unsafe { transmute::<&[MaybeUninit<u8>], &[u8]>(buf.get_unchecked(..needed)) };
            writer.write(buf)?;
            Ok(())
        }
    }

    #[derive(Debug, SchemaRead, SchemaWrite)]
    struct Skippable<T: SchemaWrite<Src = T> + for<'a> SchemaRead<'a, Dst = T>> {
        payload_len: ShortU16,
        payload: T,
    }

    impl Skippable<T: SchemaWrite<Src = T> + for<'a> SchemaRead<'a, Dst = T>> {
        fn try_new(value: T) -> WriteResult<Self> {
            Ok(Self {
                payload_len: ShortU16(
                    wincode::serialized_size(&value)?
                        .try_into()
                        .map_err(|e| WriteError::LengthEncodingOverflow("Value too long"))?,
                ),
                payload: value,
            })
        }
    }

    #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite)]
    enum MyEnum {
        #[wincode(tag = 1)]
        A { data: Skippable<u64> },
        #[wincode(tag = 2)]
        B { data: Skippable<u8> },
    }

    #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite)]
    enum MyEnum2 {
        #[wincode(tag = 3)]
        C { data: Skippable<u8> },
    }
    #[derive(Debug)]
    struct MyEnumFlexible;

    impl<'de> SchemaRead<'de> for MyEnumFlexible {
        type Dst = Option<MyEnum>;

        fn read(
            reader: &mut impl Reader<'de>,
            dst: &mut std::mem::MaybeUninit<Self::Dst>,
        ) -> wincode::ReadResult<()> {
            match MyEnum::get(reader) {
                Ok(x) => {
                    dst.write(Some(x));
                    Ok(())
                }
                Err(wincode::ReadError::InvalidTagEncoding(_)) => {
                    let length = ShortU16::get(reader)?;
                    reader.consume(length.into())?;
                    dst.write(None);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    }
    #[derive(Debug)]
    struct MyEnumFlexible2;

    impl<'de> wincode::SchemaRead<'de> for MyEnumFlexible2 {
        type Dst = Option<MyEnum2>;

        fn read(
            reader: &mut impl Reader<'de>,
            dst: &mut std::mem::MaybeUninit<Self::Dst>,
        ) -> wincode::ReadResult<()> {
            match MyEnum2::get(reader) {
                Ok(x) => {
                    dst.write(Some(x));
                    Ok(())
                }
                Err(wincode::ReadError::InvalidTagEncoding(_)) => {
                    let length = ShortU16::get(reader)?;
                    reader.consume(length.into())?;
                    dst.write(None);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    }
    #[test]
    fn test_zach() {
        let message1 = MyEnum::A {
            data: Skippable::try_new(777).unwrap(),
        };
        let message2 = MyEnum2::C {
            data: Skippable::try_new(42).unwrap(),
        };

        let buf1 = wincode::serialize(&message1).unwrap();
        let buf2 = wincode::serialize(&message2).unwrap();
        dbg!(&buf1, &buf2);
        let res = MyEnumFlexible::get(&mut buf1.as_slice());

        dbg!(res);
        let res = MyEnumFlexible2::get(&mut buf1.as_slice());
        dbg!(res);
    }
}
