use {
    crate::{short_u16::ShortU16, TlvSerialize, MAX_VALUE_LENGTH},
    bytes::BytesMut,
    std::marker::PhantomData,
    wincode::{
        containers, io::Reader, len::ShortU16Len, ReadResult, SchemaRead, SchemaWrite, WriteError,
        WriteResult,
    },
};

/// Marker trait for payloads, eligible for use in [struct LvRecord].
/// Automatically implemented for all eligible types.
pub trait LvPayload: SchemaWrite<Src = Self> + for<'a> SchemaRead<'a, Dst = Self> {}
impl<T> LvPayload for T
where
    T: SchemaWrite<Src = T>,
    for<'a> T: SchemaRead<'a, Dst = T>,
{
}

/// A Length-Value encoded entry. Can be skipped by receivers that do not
/// have a parser for a specifc T. Used to construct entries in TLV enums:
/// ```rust
/// #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite)]
/// enum MyEnum {
///    #[wincode(tag = 1)]
///    A (LvRecord<u8> ),
///    #[wincode(tag = 2)]
///    A (LvRecord<u64> ),
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq, SchemaRead, SchemaWrite)]
pub struct LvRecord<T: LvPayload> {
    payload_len: ShortU16,
    // direct access to payload is blocked to ensure payload_len is always in sync
    payload: T,
}

const MAX_VALUE_LENGTH: usize = 1280;

impl<T: LvPayload> LvRecord<T> {
    pub fn try_new(value: T) -> WriteResult<Self> {
        let length = wincode::serialized_size(&value)?
            .try_into()
            .map_err(|e| WriteError::LengthEncodingOverflow("Value too long"))?;
        if length > MAX_VALUE_LENGTH {
            return Err(WriteError::LengthEncodingOverflow("Value too long"));
        }
        Ok(Self {
            payload_len: ShortU16(length),
            payload: value,
        })
    }

    /// simpler constructor for unittests
    #[cfg(test)]
    pub fn new(value: T) -> Self {
        Self::try_new(value).expect("Construction should succeed")
    }

    pub fn payload(&self) -> &T {
        return self.payload;
    }
}

/// A high level interface to perform TLV parsing via wincode.
///
/// ```rust
/// #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite)]
/// enum MyEnum {
///    #[wincode(tag = 3)]
///    A (LvRecord<u8> ),
/// }
/// let message = MyEnum::A (
///    LvRecord::try_new(42).unwrap(),
/// );
/// let buf = wincode::serialize(&message).unwrap();
/// let res: Result<Option<MyEnum>, _> = TlvSchemaReader::get(&mut buf);
/// ```
#[derive(Debug)]
pub struct TlvSchemaReader<T>(PhantomData<T>);

impl<'de, T: SchemaRead<'de, Dst = T>> SchemaRead<'de> for TlvSchemaReader<T> {
    type Dst = Option<T>;

    fn read(
        reader: &mut impl Reader<'de>,
        dst: &mut std::mem::MaybeUninit<Self::Dst>,
    ) -> wincode::ReadResult<()> {
        match T::get(reader) {
            Ok(x) => {
                dst.write(Some(x));
                Ok(())
            }
            Err(wincode::ReadError::InvalidTagEncoding(_)) => {
                let length = ShortU16::get(reader)?;
                if length > MAX_VALUE_LENGTH {
                    return Err(wincode::ReadError::LengthEncodingOverflow("Value too long"));
                }
                reader.consume(length.into())?;
                dst.write(None);
                Ok(())
            }
            Err(e) => Err(e),
        }
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
        crate::{
            lv_record::{LvRecord, TlvSchemaReader},
            short_u16::ShortU16,
            TlvEncodeError,
        },
        bytes::BytesMut,
        std::mem::{transmute, MaybeUninit},
        wincode::{
            error::{read_length_encoding_overflow, write_length_encoding_overflow},
            io::Reader,
            ReadResult, SchemaRead, SchemaWrite, WriteError, WriteResult,
        },
    };

    #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite)]
    enum MyEnum {
        #[wincode(tag = 1)]
        A(LvRecord<u64>),
        #[wincode(tag = 2)]
        B(LvRecord<u8>),
    }

    #[derive(Debug, wincode::SchemaRead, wincode::SchemaWrite)]
    enum MyEnum2 {
        #[wincode(tag = 3)]
        C(LvRecord<u8>),
    }

    #[test]
    fn test_zach() {
        let message1 = MyEnum::A(LvRecord::try_new(777).unwrap());
        let message2 = MyEnum2::C(LvRecord::try_new(42).unwrap());

        let buf1 = wincode::serialize(&message1).unwrap();
        let buf2 = wincode::serialize(&message2).unwrap();
        dbg!(&buf1, &buf2);
        let res: Result<Option<MyEnum>, _> = TlvSchemaReader::get(&mut buf1.as_slice());

        dbg!(res);
        let res: Result<Option<MyEnum2>, _> = TlvSchemaReader::get(&mut buf1.as_slice());

        dbg!(res);
    }
}
