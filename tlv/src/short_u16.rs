use {
    core::ptr,
    solana_short_vec::decode_shortu16_len,
    std::mem::{transmute, MaybeUninit},
    wincode::{
        error::{read_length_encoding_overflow, write_length_encoding_overflow},
        io::Reader,
        ReadResult, SchemaRead, SchemaWrite, WriteResult,
    },
};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[repr(transparent)]
pub struct ShortU16(pub u16);

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
    unsafe {
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
        Ok(())
    }
}

impl SchemaWrite for ShortU16 {
    type Src = ShortU16;

    fn size_of(src: &Self::Src) -> wincode::WriteResult<usize> {
        try_short_u16_bytes_needed(src.0)
    }

    fn write(writer: &mut impl wincode::io::Writer, src: &Self::Src) -> wincode::WriteResult<()> {
        let src = src.0;
        let needed = short_u16_bytes_needed(src);
        let mut buf = [MaybeUninit::<u8>::uninit(); 3];
        // SAFETY: short_u16 uses a maximum of 3 bytes, so the buffer is always large enough.
        unsafe { encode_short_u16(buf.as_mut_ptr().cast(), needed, src) };
        // SAFETY: encode_short_u16 writes exactly `needed` bytes.
        let buf = unsafe { transmute::<&[MaybeUninit<u8>], &[u8]>(buf.get_unchecked(..needed)) };
        writer.write(buf)?;
        Ok(())
    }
}
