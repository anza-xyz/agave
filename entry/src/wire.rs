//! Efficient serialization and deserialization of the [`Entry`] wire format.
//!
//! # Background
//!
//! A bincode serialized [`Entry`], whose serialization format and scheme is
//! defined in large part by the solana-sdk, is the de-facto wire format for
//! turbine. This makes it quite difficult to change, so any attempt at
//! improving the performance of serialization / deserialization must operate
//! on that format, at least until some larger effort is made to overhaul it.
//!
//! The existing implementation suffers from all the typical serde inefficiencies,
//! like all the copying that comes with Rust's poor NRVO. Worse, however, is
//! that byte arrays, byte vectors, and sequences thereof are all encoded as
//! individual sequence elements. This means at _least_ `N` copies where `N` is
//! the length of the sequence, not including the implicit copies from lack of
//! inline initialization / guarantees of RVO. For example, given the following
//! byte array:
//!
//! ```
//! let signature = [0; 64];
//! ```
//!
//! Each byte will be visited and copied _individually_ rather than as a single
//! unit. Worse, if this type of byte array is part of a larger sequence, like
//! a `Vec<Signature>`, that visitation inneficiency is compounded by the
//! length of the sequence _and_ stack allocated before being written
//! into the Vec's heap memory.
//!
//! # This module
//!
//! This module provides an efficient implementaton of the serialization and
//! deserialization of the [`Entry`] wire format. In particular:
//! - byte arrays / byte vectors (and sequences thereof) are encoded as raw bytes.
//! - intermediate types are written to in place.
//! - vec memory is written to in place.

use {
    crate::entry::Entry,
    solana_address::ADDRESS_BYTES,
    solana_hash::HASH_BYTES,
    solana_message::{
        legacy::Message as LegacyMessage,
        v0::{Message as V0Message, MessageAddressTableLookup},
        MessageHeader, VersionedMessage, MESSAGE_VERSION_PREFIX,
    },
    solana_short_vec::decode_shortu16_len,
    solana_signature::SIGNATURE_BYTES,
    solana_transaction::{versioned::VersionedTransaction, CompiledInstruction},
    std::{borrow::Borrow, mem::MaybeUninit, ops::Add, ptr},
};

/// Behavior for heterogenous sequence length encoding.
///
/// The wire format has to deal with two different length encoding schemes for
/// sequences.
/// - All sequences defined within the solana-sdk use the variable length
///   [`solana_short_vec`] encoding.
/// - Sequences defined outside of the solana-sdk use bincode's default fixint encoding.
///
/// This trait abstracts over these two different encoding schemes, which simpifies
/// serialization / deserialization over sequences.
pub trait SeqLen {
    /// Read the length of a sequence from the reader.
    fn get_len(reader: &mut Reader) -> bincode::Result<usize>;
    /// Write the length of a sequence to the writer.
    fn encode_len(writer: &mut Writer, len: usize) -> bincode::Result<()>;
    /// Calculate the number of bytes needed to encode the given length.
    fn bytes_needed(len: usize) -> bincode::Result<usize>;
}

struct BincodeLen;

impl SeqLen for BincodeLen {
    #[inline(always)]
    fn get_len(reader: &mut Reader) -> bincode::Result<usize> {
        reader.get_u64().map(|len| len as usize)
    }

    #[inline(always)]
    fn encode_len(writer: &mut Writer, len: usize) -> bincode::Result<()> {
        writer.write_u64(len as u64)
    }

    #[inline(always)]
    fn bytes_needed(_len: usize) -> bincode::Result<usize> {
        Ok(size_of::<u64>())
    }
}

struct ShortU16Len;

/// Branchless computation of the number of bytes needed to encode a short u16.
#[inline(always)]
fn short_u16_bytes_needed(len: u16) -> usize {
    1 + (len >= 0x80) as usize + (len >= 0x4000) as usize
}

#[cold]
fn error_invalid_short_u16() -> bincode::Error {
    bincode::ErrorKind::Custom("length exceeds u16::MAX".to_string()).into()
}

#[cold]
fn error_size_limit() -> bincode::Error {
    bincode::ErrorKind::SizeLimit.into()
}

#[inline(always)]
fn try_short_u16_bytes_needed<T: TryInto<u16>>(len: T) -> bincode::Result<usize> {
    match len.try_into() {
        Ok(len) => Ok(short_u16_bytes_needed(len)),
        Err(_) => Err(error_invalid_short_u16()),
    }
}

/// Encode a short u16 into the given buffer.
///
/// # Safety
///
/// - `dst` must be a valid for writes
/// - `dst` must be valid for `needed` bytes
#[inline(always)]
unsafe fn encode_short_u16(dst: *mut u8, needed: usize, len: u16) {
    match needed {
        1 => std::ptr::write(dst, len as u8),
        2 => {
            std::ptr::write(dst, ((len & 0x7f) as u8) | 0x80);
            std::ptr::write(dst.add(1), (len >> 7) as u8);
        }
        3 => {
            std::ptr::write(dst, ((len & 0x7f) as u8) | 0x80);
            std::ptr::write(dst.add(1), (((len >> 7) & 0x7f) as u8) | 0x80);
            std::ptr::write(dst.add(2), (len >> 14) as u8);
        }
        _ => unreachable!(),
    }
}

impl SeqLen for ShortU16Len {
    #[inline(always)]
    fn get_len(cursor: &mut Reader) -> bincode::Result<usize> {
        let Ok((len, read)) = decode_shortu16_len(&cursor.cursor[cursor.pos..]) else {
            return Err(error_invalid_short_u16());
        };
        cursor.pos += read;
        Ok(len)
    }

    #[inline(always)]
    fn encode_len(writer: &mut Writer, len: usize) -> bincode::Result<()> {
        if len > u16::MAX as usize {
            return Err(error_invalid_short_u16());
        }

        let len = len as u16;
        let needed = short_u16_bytes_needed(len);
        let cap = writer.buffer.capacity();
        let free = cap.wrapping_sub(writer.pos);
        if free < needed {
            return Err(error_size_limit());
        }

        // SAFETY: `writer.buffer` is valid for `needed` bytes
        unsafe {
            let dst = writer.buffer.as_mut_ptr().add(writer.pos);
            encode_short_u16(dst, needed, len);
        };
        writer.pos += needed;

        Ok(())
    }

    #[inline(always)]
    fn bytes_needed(len: usize) -> bincode::Result<usize> {
        try_short_u16_bytes_needed(len)
    }
}

pub struct Reader<'a> {
    cursor: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: bytes,
            pos: 0,
        }
    }

    /// Copy exactly `len` bytes from the cursor into `buf`.
    #[inline]
    fn read_exact(&mut self, buf: *mut u8, len: usize) -> bincode::Result<()> {
        if self.pos + len > self.cursor.len() {
            return Err(error_size_limit());
        }
        unsafe {
            ptr::copy_nonoverlapping(self.cursor.as_ptr().add(self.pos), buf, len);
        }
        self.pos += len;
        Ok(())
    }

    /// Copy exactly `size_of::<T>()` bytes from the cursor into `ptr`.
    #[inline(always)]
    fn read_t<T>(&mut self, ptr: *mut T) -> bincode::Result<()> {
        self.read_exact(ptr as *mut u8, size_of::<T>())
    }

    /// Copy exactly `size_of::<T>()` bytes from the cursor into a new `T` on the stack.
    #[inline(always)]
    fn get_t<T>(&mut self) -> bincode::Result<T> {
        let mut t = MaybeUninit::<T>::uninit();
        self.read_t(t.as_mut_ptr())?;
        Ok(unsafe { t.assume_init() })
    }

    #[inline(always)]
    fn read_u64(&mut self, ptr: *mut u64) -> bincode::Result<()> {
        self.read_t(ptr)?;
        // bincode defaults to little endian encoding
        #[cfg(target_endian = "big")]
        {
            // SAFETY: ptr is initialized by read_t
            let val = unsafe { &mut *ptr };
            *val = val.swap_bytes();
        }

        Ok(())
    }

    #[inline(always)]
    fn get_u64(&mut self) -> bincode::Result<u64> {
        let mut u64 = MaybeUninit::<u64>::uninit();
        self.read_u64(u64.as_mut_ptr())?;
        // SAFETY: u64 is initialized by read_u64
        Ok(unsafe { u64.assume_init() })
    }

    /// Read a sequence of `T`s from the cursor into `ptr`.
    ///
    /// This provides a `*mut T` for each slot in the allocated Vec
    /// to facilitate in-place writing of Vec memory.
    ///
    /// Length encoding can be configured via the `Len` parameter.
    ///
    /// Prefer [`Self::read_byte_seq`] for sequences raw bytes.
    fn read_seq<T, F, Len>(
        &mut self,
        ptr: *mut Vec<T>,
        _len: Len,
        parse_t: F,
    ) -> bincode::Result<()>
    where
        F: Fn(&mut Reader<'a>, *mut T) -> bincode::Result<()>,
        Len: SeqLen,
    {
        let len = Len::get_len(self)?;
        let mut vec: Vec<T> = Vec::with_capacity(len);
        // Get a raw pointer to the Vec memory to facilitate in-place writing.
        let mut vec_ptr = vec.spare_capacity_mut().as_mut_ptr();
        for i in 0..len {
            // Yield the current slot to the caller.
            parse_t(self, vec_ptr as *mut T)?;

            unsafe {
                vec_ptr = vec_ptr.add(1);
                // Set the len for drop safety.
                vec.set_len(i + 1);
            }
        }
        unsafe {
            ptr::write(ptr, vec);
        }
        Ok(())
    }

    /// Read a sequence of bytes or a sequence of fixed length byte arrays from the cursor into `ptr`.
    ///
    /// This reads the entire sequence at once, rather than yielding each element to the caller.
    ///
    /// Should be used with types representable by raw bytes, like `Vec<u8>` or `Vec<[u8; N]>`.
    ///
    /// Length encoding can be configured via the `Len` parameter.
    fn read_byte_seq<T, Len>(&mut self, ptr: *mut Vec<T>, _len: Len) -> bincode::Result<()>
    where
        Len: SeqLen,
    {
        let len = Len::get_len(self)?;
        let mut vec: Vec<T> = Vec::with_capacity(len);
        let vec_ptr = vec.spare_capacity_mut().as_mut_ptr();
        self.read_exact(vec_ptr as *mut u8, len * size_of::<T>())?;
        unsafe {
            vec.set_len(len);
            ptr::write(ptr, vec);
        }
        Ok(())
    }
}

fn de_compiled_instruction(
    cursor: &mut Reader,
    ptr: *mut CompiledInstruction,
) -> bincode::Result<()> {
    let (program_id_index_ptr, accounts_ptr, data_ptr) = unsafe {
        (
            &raw mut ((*ptr).program_id_index),
            &raw mut ((*ptr).accounts),
            &raw mut ((*ptr).data),
        )
    };
    cursor.read_t(program_id_index_ptr)?;
    cursor.read_byte_seq(accounts_ptr, ShortU16Len)?;
    cursor.read_byte_seq(data_ptr, ShortU16Len)?;
    Ok(())
}

fn de_legacy_message(cursor: &mut Reader, ptr: *mut LegacyMessage) -> bincode::Result<()> {
    let (account_keys_ptr, recent_blockhash_ptr, instructions_ptr) = unsafe {
        (
            &raw mut (*ptr).account_keys,
            &raw mut (*ptr).recent_blockhash,
            &raw mut (*ptr).instructions,
        )
    };
    cursor.read_byte_seq(account_keys_ptr, ShortU16Len)?;
    cursor.read_t(recent_blockhash_ptr)?;
    cursor.read_seq(instructions_ptr, ShortU16Len, de_compiled_instruction)?;
    Ok(())
}

fn de_address_table_lookup(
    cursor: &mut Reader,
    ptr: *mut MessageAddressTableLookup,
) -> bincode::Result<()> {
    let (account_key_ptr, writable_indexes_ptr, readonly_indexes_ptr) = unsafe {
        (
            &raw mut ((*ptr).account_key),
            &raw mut ((*ptr).writable_indexes),
            &raw mut ((*ptr).readonly_indexes),
        )
    };
    cursor.read_t(account_key_ptr)?;
    cursor.read_byte_seq(writable_indexes_ptr, ShortU16Len)?;
    cursor.read_byte_seq(readonly_indexes_ptr, ShortU16Len)?;
    Ok(())
}

fn de_v0_message(cursor: &mut Reader, ptr: *mut V0Message) -> bincode::Result<()> {
    let (account_keys_ptr, recent_blockhash_ptr, instructions_ptr, address_table_lookups_ptr) = unsafe {
        (
            &raw mut (*ptr).account_keys,
            &raw mut (*ptr).recent_blockhash,
            &raw mut (*ptr).instructions,
            &raw mut (*ptr).address_table_lookups,
        )
    };
    cursor.read_byte_seq(account_keys_ptr, ShortU16Len)?;
    cursor.read_t(recent_blockhash_ptr)?;
    cursor.read_seq(instructions_ptr, ShortU16Len, de_compiled_instruction)?;
    cursor.read_seq(
        address_table_lookups_ptr,
        ShortU16Len,
        de_address_table_lookup,
    )?;
    Ok(())
}

/// See [`solana_message::VersionedMessage`] for more details.
fn de_versioned_message(cursor: &mut Reader, ptr: *mut VersionedMessage) -> bincode::Result<()> {
    // From `solana_message`:
    //
    // If the first bit is set, the remaining 7 bits will be used to determine
    // which message version is serialized starting from version `0`. If the first
    // is bit is not set, all bytes are used to encode the legacy `Message`
    // format.
    let variant = cursor.get_t::<u8>()?;

    #[cold]
    fn invalid_version(version: u8) -> bincode::Error {
        bincode::ErrorKind::InvalidTagEncoding(version as usize).into()
    }

    if variant & MESSAGE_VERSION_PREFIX != 0 {
        let version = variant & !MESSAGE_VERSION_PREFIX;
        match version {
            0 => {
                let mut msg = MaybeUninit::<V0Message>::uninit();
                let msg_ptr = msg.as_mut_ptr();
                let num_required_signatures_ptr =
                    unsafe { &raw mut (*msg_ptr).header.num_required_signatures };
                // header is serialized as 3 contiguous bytes.
                cursor.read_t(num_required_signatures_ptr as *mut [u8; 3])?;
                de_v0_message(cursor, msg_ptr)?;
                unsafe {
                    ptr::write(ptr, VersionedMessage::V0(msg.assume_init()));
                }
            }
            127 => {
                return Err(invalid_version(127));
            }
            _ => {
                return Err(invalid_version(version));
            }
        }
    } else {
        let mut msg = MaybeUninit::<LegacyMessage>::uninit();
        let msg_ptr = msg.as_mut_ptr();
        let (num_required_signatures_ptr, num_readonly_signed_accounts_ptr) = unsafe {
            (
                &raw mut (*msg_ptr).header.num_required_signatures,
                &raw mut (*msg_ptr).header.num_readonly_signed_accounts,
            )
        };
        unsafe {
            ptr::write(num_required_signatures_ptr, variant);
        }
        // read the next 2 contiguous bytes.
        cursor.read_t(num_readonly_signed_accounts_ptr as *mut [u8; 2])?;
        de_legacy_message(cursor, msg_ptr)?;
        unsafe {
            ptr::write(ptr, VersionedMessage::Legacy(msg.assume_init()));
        }
    }

    Ok(())
}

fn de_versioned_transaction(
    cursor: &mut Reader,
    ptr: *mut VersionedTransaction,
) -> bincode::Result<()> {
    let (signatures_ptr, message_ptr) =
        unsafe { (&raw mut (*ptr).signatures, &raw mut (*ptr).message) };
    cursor.read_byte_seq(signatures_ptr, ShortU16Len)?;
    de_versioned_message(cursor, message_ptr)?;
    Ok(())
}

fn de_entry(cursor: &mut Reader, ptr: *mut Entry) -> bincode::Result<()> {
    let (num_hashes_ptr, hash_ptr, txs_ptr) = unsafe {
        (
            &raw mut (*ptr).num_hashes,
            &raw mut (*ptr).hash,
            &raw mut (*ptr).transactions,
        )
    };

    cursor.read_u64(num_hashes_ptr)?;
    cursor.read_t(hash_ptr)?;
    cursor.read_seq(txs_ptr, BincodeLen, de_versioned_transaction)?;

    Ok(())
}

pub trait Deserialize {
    fn deserialize(bytes: &[u8]) -> bincode::Result<Self>
    where
        Self: Sized;
}

impl Deserialize for Entry {
    #[inline(always)]
    fn deserialize(bytes: &[u8]) -> bincode::Result<Self> {
        deserialize_entry(bytes)
    }
}

impl Deserialize for Vec<Entry> {
    #[inline(always)]
    fn deserialize(bytes: &[u8]) -> bincode::Result<Self> {
        deserialize_entry_multi(bytes)
    }
}

pub fn deserialize_entry(bytes: &[u8]) -> bincode::Result<Entry> {
    let mut cursor = Reader::new(bytes);
    let mut entry = MaybeUninit::<Entry>::uninit();
    let entry_ptr = entry.as_mut_ptr();
    de_entry(&mut cursor, entry_ptr)?;
    Ok(unsafe { entry.assume_init() })
}

pub fn deserialize_entry_multi(slice: &[u8]) -> bincode::Result<Vec<Entry>> {
    let mut cursor = Reader::new(slice);
    let mut entries = MaybeUninit::<Vec<Entry>>::uninit();
    cursor.read_seq(entries.as_mut_ptr(), BincodeLen, de_entry)?;
    Ok(unsafe { entries.assume_init() })
}

pub struct Writer<'a> {
    buffer: &'a mut Vec<u8>,
    pos: usize,
}

impl<'a> Writer<'a> {
    fn new(buffer: &'a mut Vec<u8>) -> Self {
        Self { buffer, pos: 0 }
    }

    fn finish(&mut self) {
        unsafe {
            self.buffer.set_len(self.pos);
        }
    }

    /// Write exactly `len` bytes from `buf` into the internal buffer.
    #[inline(always)]
    fn write(&mut self, buf: *const u8, len: usize) -> bincode::Result<()> {
        if self.pos + len > self.buffer.capacity() {
            return Err(error_size_limit());
        }
        unsafe {
            ptr::copy_nonoverlapping(buf, self.buffer.as_mut_ptr().add(self.pos), len);
        }
        self.pos += len;
        Ok(())
    }

    /// Write T into the internal buffer.
    #[inline(always)]
    fn write_t<T>(&mut self, value: &T) -> bincode::Result<()> {
        self.write(value as *const T as *const u8, size_of::<T>())
    }

    #[inline(always)]
    fn write_u64(&mut self, value: u64) -> bincode::Result<()> {
        // bincode defaults to little endian encoding
        let val = value.to_le_bytes();

        self.write_t(&val)?;
        Ok(())
    }

    /// Write a byte slice into the internal buffer.
    #[inline(always)]
    fn write_bytes(&mut self, value: &[u8]) -> bincode::Result<()> {
        self.write(value.as_ptr(), value.len())
    }

    /// Write a sequence of `T`s into the internal buffer.
    ///
    /// Length encoding can be configured via the `Len` parameter.
    ///
    /// Prefer [`Self::write_byte_seq`] for sequences of raw bytes.
    #[inline(always)]
    fn write_seq<T, F, Len>(&mut self, value: &[T], _len: Len, write_t: F) -> bincode::Result<()>
    where
        F: Fn(&mut Writer<'a>, &T) -> bincode::Result<()>,
        Len: SeqLen,
    {
        Len::encode_len(self, value.len())?;
        for item in value {
            write_t(self, item)?;
        }
        Ok(())
    }

    /// Write a sequence of raw bytes into the internal buffer.
    ///
    /// Should be used with types representable by raw bytes, like `Vec<u8>` or `Vec<[u8; N]>`.
    ///
    /// This writes the entire sequence at once, rather than yielding each element to the caller.
    ///
    /// Length encoding can be configured via the `Len` parameter.
    #[inline(always)]
    fn write_byte_seq<T, Len>(&mut self, value: &[T], _len: Len) -> bincode::Result<()>
    where
        Len: SeqLen,
    {
        Len::encode_len(self, value.len())?;
        self.write(value.as_ptr() as *const u8, size_of_val(value))
    }
}

fn se_compiled_instruction(
    writer: &mut Writer,
    value: &CompiledInstruction,
) -> bincode::Result<()> {
    writer.write_t(&value.program_id_index)?;
    writer.write_byte_seq(&value.accounts, ShortU16Len)?;
    writer.write_byte_seq(&value.data, ShortU16Len)?;
    Ok(())
}

#[inline(always)]
fn se_header(writer: &mut Writer, value: &MessageHeader) -> bincode::Result<()> {
    writer.write_bytes(&[
        value.num_required_signatures,
        value.num_readonly_signed_accounts,
        value.num_readonly_unsigned_accounts,
    ])?;
    Ok(())
}

fn se_legacy_message(writer: &mut Writer, value: &LegacyMessage) -> bincode::Result<()> {
    se_header(writer, &value.header)?;
    writer.write_byte_seq(&value.account_keys, ShortU16Len)?;
    writer.write_bytes(value.recent_blockhash.as_ref())?;
    writer.write_seq(&value.instructions, ShortU16Len, se_compiled_instruction)?;
    Ok(())
}

fn se_address_table_lookup(
    writer: &mut Writer,
    value: &MessageAddressTableLookup,
) -> bincode::Result<()> {
    writer.write_bytes(value.account_key.as_ref())?;
    writer.write_byte_seq(&value.writable_indexes, ShortU16Len)?;
    writer.write_byte_seq(&value.readonly_indexes, ShortU16Len)?;
    Ok(())
}

fn se_v0_message(writer: &mut Writer, value: &V0Message) -> bincode::Result<()> {
    writer.write_t(&MESSAGE_VERSION_PREFIX)?;
    se_header(writer, &value.header)?;
    writer.write_byte_seq(&value.account_keys, ShortU16Len)?;
    writer.write_bytes(value.recent_blockhash.as_ref())?;
    writer.write_seq(&value.instructions, ShortU16Len, se_compiled_instruction)?;
    writer.write_seq(
        &value.address_table_lookups,
        ShortU16Len,
        se_address_table_lookup,
    )?;
    Ok(())
}

#[inline(always)]
fn se_versioned_message(writer: &mut Writer, value: &VersionedMessage) -> bincode::Result<()> {
    match value {
        VersionedMessage::Legacy(message) => se_legacy_message(writer, message),
        VersionedMessage::V0(message) => se_v0_message(writer, message),
    }
}

fn se_versioned_transaction(
    writer: &mut Writer,
    value: &VersionedTransaction,
) -> bincode::Result<()> {
    writer.write_byte_seq(&value.signatures, ShortU16Len)?;
    se_versioned_message(writer, &value.message)?;
    Ok(())
}

fn se_entry(writer: &mut Writer, value: &Entry) -> bincode::Result<()> {
    writer.write_u64(value.num_hashes)?;
    writer.write_bytes(value.hash.as_ref())?;
    writer.write_seq(&value.transactions, BincodeLen, se_versioned_transaction)?;
    Ok(())
}

pub trait Serialize {
    fn serialize(&self) -> bincode::Result<Vec<u8>>;
    fn serialized_size(&self) -> bincode::Result<u64>;
}

impl<E> Serialize for E
where
    E: Borrow<Entry>,
{
    #[inline(always)]
    fn serialize(&self) -> bincode::Result<Vec<u8>> {
        serialize_entry(self.borrow())
    }

    #[inline(always)]
    fn serialized_size(&self) -> bincode::Result<u64> {
        serialized_size(self.borrow())
    }
}

impl Serialize for &[Entry] {
    #[inline(always)]
    fn serialize(&self) -> bincode::Result<Vec<u8>> {
        serialize_entry_multi(self)
    }

    #[inline(always)]
    fn serialized_size(&self) -> bincode::Result<u64> {
        serialized_size_multi(self)
    }
}

trait TrySum<T, E> {
    fn try_sum(self) -> Result<T, E>;
}

impl<I, T, E> TrySum<T, E> for I
where
    T: Add<Output = T> + Default,
    I: Iterator<Item = Result<T, E>>,
{
    #[inline(always)]
    fn try_sum(mut self) -> Result<T, E> {
        self.try_fold(T::default(), |acc, x| Ok(acc + x?))
    }
}

#[inline(always)]
fn size_of_seq<T, Len, F>(seq: &[T], _len: Len, size_of_t: F) -> bincode::Result<usize>
where
    Len: SeqLen,
    F: Fn(&T) -> bincode::Result<usize>,
{
    Ok(Len::bytes_needed(seq.len())? + seq.iter().map(size_of_t).try_sum()?)
}

#[inline(always)]
fn size_of_seq_fixed<T, Len>(seq: &[T], _len: Len, fixed_size: usize) -> bincode::Result<usize>
where
    Len: SeqLen,
{
    Ok(Len::bytes_needed(seq.len())? + seq.len() * fixed_size)
}

#[inline(always)]
fn size_of_seq_bytes<T, Len>(seq: &[T], _len: Len) -> bincode::Result<usize>
where
    Len: SeqLen,
{
    size_of_seq_fixed(seq, _len, 1)
}

fn size_of_instruction(instruction: &CompiledInstruction) -> bincode::Result<usize> {
    Ok(size_of::<u8>()
        + size_of_seq_bytes(&instruction.accounts, ShortU16Len)?
        + size_of_seq_bytes(&instruction.data, ShortU16Len)?)
}

fn size_of_legacy_message(message: &LegacyMessage) -> bincode::Result<usize> {
    Ok(size_of::<MessageHeader>()
        + size_of_seq_fixed(&message.account_keys, ShortU16Len, ADDRESS_BYTES)?
        + HASH_BYTES
        + size_of_seq(&message.instructions, ShortU16Len, size_of_instruction)?)
}

fn size_of_address_table_lookup(lookup: &MessageAddressTableLookup) -> bincode::Result<usize> {
    Ok(ADDRESS_BYTES
        + size_of_seq_bytes(&lookup.writable_indexes, ShortU16Len)?
        + size_of_seq_bytes(&lookup.readonly_indexes, ShortU16Len)?)
}

fn size_of_v0_message(message: &V0Message) -> bincode::Result<usize> {
    Ok(size_of::<MessageHeader>()
        + size_of_seq_fixed(&message.account_keys, ShortU16Len, ADDRESS_BYTES)?
        + HASH_BYTES
        + size_of_seq(&message.instructions, ShortU16Len, size_of_instruction)?
        + size_of_seq(
            &message.address_table_lookups,
            ShortU16Len,
            size_of_address_table_lookup,
        )?)
}

pub fn serialized_size(value: &Entry) -> bincode::Result<u64> {
    // num_hashes
    let mut size = size_of::<u64>();
    // hash
    size += HASH_BYTES;
    size += BincodeLen::bytes_needed(value.transactions.len())?;
    for tx in &value.transactions {
        size += size_of_seq_fixed(&tx.signatures, ShortU16Len, SIGNATURE_BYTES)?;
        match &tx.message {
            VersionedMessage::Legacy(message) => {
                size += size_of_legacy_message(message)?;
            }
            VersionedMessage::V0(message) => {
                // version prefix
                size += 1;
                size += size_of_v0_message(message)?;
            }
        }
    }
    Ok(size as u64)
}

#[inline]
pub fn serialized_size_multi(entries: &[Entry]) -> bincode::Result<u64> {
    let mut size = BincodeLen::bytes_needed(entries.len())? as u64;
    for entry in entries {
        size += serialized_size(entry)?;
    }
    Ok(size)
}

pub fn serialize_entry(entry: &Entry) -> bincode::Result<Vec<u8>> {
    let size = serialized_size(entry)?;
    let mut buffer = Vec::with_capacity(size as usize);
    let mut writer = Writer::new(&mut buffer);
    se_entry(&mut writer, entry)?;
    writer.finish();
    Ok(buffer)
}

pub fn serialize_entry_multi(entries: &[Entry]) -> bincode::Result<Vec<u8>> {
    let size = serialized_size_multi(entries)?;
    let mut buffer = Vec::with_capacity(size as usize);
    let mut writer = Writer::new(&mut buffer);
    writer.write_seq(entries, BincodeLen, se_entry)?;
    writer.finish();
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        proptest::prelude::*,
        solana_address::{Address, ADDRESS_BYTES},
        solana_hash::{Hash, HASH_BYTES},
        solana_message::MessageHeader,
        solana_short_vec::ShortU16,
        solana_signature::{Signature, SIGNATURE_BYTES},
    };

    fn strat_byte_vec(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..=max_len)
    }

    fn strat_signature() -> impl Strategy<Value = Signature> {
        any::<[u8; SIGNATURE_BYTES]>().prop_map(Signature::from)
    }

    fn strat_address() -> impl Strategy<Value = Address> {
        any::<[u8; ADDRESS_BYTES]>().prop_map(Address::new_from_array)
    }

    fn strat_hash() -> impl Strategy<Value = Hash> {
        any::<[u8; HASH_BYTES]>().prop_map(Hash::new_from_array)
    }

    fn strat_message_header() -> impl Strategy<Value = MessageHeader> {
        (0u8..128, any::<u8>(), any::<u8>()).prop_map(|(a, b, c)| MessageHeader {
            num_required_signatures: a,
            num_readonly_signed_accounts: b,
            num_readonly_unsigned_accounts: c,
        })
    }

    fn strat_compiled_instruction() -> impl Strategy<Value = CompiledInstruction> {
        (any::<u8>(), strat_byte_vec(256), strat_byte_vec(256)).prop_map(
            |(program_id_index, accounts, data)| {
                CompiledInstruction::new_from_raw_parts(program_id_index, accounts, data)
            },
        )
    }

    fn strat_address_table_lookup() -> impl Strategy<Value = MessageAddressTableLookup> {
        (strat_address(), strat_byte_vec(256), strat_byte_vec(256)).prop_map(
            |(account_key, writable_indexes, readonly_indexes)| MessageAddressTableLookup {
                account_key,
                writable_indexes,
                readonly_indexes,
            },
        )
    }

    fn strat_legacy_message() -> impl Strategy<Value = LegacyMessage> {
        (
            strat_message_header(),
            proptest::collection::vec(strat_address(), 0..=16),
            strat_hash(),
            proptest::collection::vec(strat_compiled_instruction(), 0..=16),
        )
            .prop_map(|(header, account_keys, recent_blockhash, instructions)| {
                LegacyMessage {
                    header,
                    account_keys,
                    recent_blockhash,
                    instructions,
                }
            })
    }

    fn strat_v0_message() -> impl Strategy<Value = V0Message> {
        (
            strat_message_header(),
            proptest::collection::vec(strat_address(), 0..=16),
            strat_hash(),
            proptest::collection::vec(strat_compiled_instruction(), 0..=16),
            proptest::collection::vec(strat_address_table_lookup(), 0..=8),
        )
            .prop_map(
                |(header, account_keys, recent_blockhash, instructions, address_table_lookups)| {
                    V0Message {
                        header,
                        account_keys,
                        recent_blockhash,
                        instructions,
                        address_table_lookups,
                    }
                },
            )
    }

    fn strat_versioned_message() -> impl Strategy<Value = VersionedMessage> {
        prop_oneof![
            strat_legacy_message().prop_map(VersionedMessage::Legacy),
            strat_v0_message().prop_map(VersionedMessage::V0),
        ]
    }

    fn strat_versioned_transaction() -> impl Strategy<Value = VersionedTransaction> {
        (
            proptest::collection::vec(strat_signature(), 0..=16),
            strat_versioned_message(),
        )
            .prop_map(|(signatures, message)| VersionedTransaction {
                signatures,
                message,
            })
    }

    fn strat_entry() -> impl Strategy<Value = Entry> {
        (
            any::<u64>(),
            strat_hash(),
            proptest::collection::vec(strat_versioned_transaction(), 0..=8),
        )
            .prop_map(|(num_hashes, hash, transactions)| Entry {
                num_hashes,
                hash,
                transactions,
            })
    }

    fn strat_entries() -> impl Strategy<Value = Vec<Entry>> {
        proptest::collection::vec(strat_entry(), 0..=4)
    }

    fn our_short_u16_encode(len: u16) -> Vec<u8> {
        let needed = short_u16_bytes_needed(len);
        let mut buf = Vec::with_capacity(needed);
        unsafe {
            encode_short_u16(buf.as_mut_ptr(), needed, len);
            buf.set_len(needed);
        }
        buf
    }

    proptest! {
        #[test]
        fn encode_u16_equivalence(len in 0..=u16::MAX) {
            let our = our_short_u16_encode(len);
            let bincode = bincode::serialize(&ShortU16(len)).unwrap();
            prop_assert_eq!(our, bincode);
        }

        #[test]
        fn serialized_size_equivalence(entry in strat_entry()) {
            let serialized = bincode::serialized_size(&entry).unwrap();
            let size = serialized_size(&entry).unwrap();
            prop_assert_eq!(serialized, size);

        }

        #[test]
        fn serialized_size_multi_equivalence(entries in strat_entries()) {
            let serialized = bincode::serialized_size(&entries).unwrap();
            let size = serialized_size_multi(&entries).unwrap();
            prop_assert_eq!(serialized, size);
        }

        #[test]
        fn de_equivalence(entry in strat_entry()) {
            let serialized = bincode::serialize(&entry).unwrap();
            let deserialized: Entry = deserialize_entry(&serialized).unwrap();
            prop_assert_eq!(entry, deserialized);
        }

        #[test]
        fn de_multi_equivalence(entries in strat_entries()) {
            let serialized = bincode::serialize(&entries).unwrap();
            let deserialized: Vec<Entry> = deserialize_entry_multi(&serialized).unwrap();
            prop_assert_eq!(entries, deserialized);
        }

        #[test]
        fn ser_equivalence(entry in strat_entry()) {
            let serialized = serialize_entry(&entry).unwrap();
            prop_assert_eq!(serialized, bincode::serialize(&entry).unwrap());
        }

        #[test]
        fn ser_multi_equivalence(entries in strat_entries()) {
            let serialized = serialize_entry_multi(&entries).unwrap();
            prop_assert_eq!(serialized, bincode::serialize(&entries).unwrap());
        }

        #[test]
        fn roundtrip(entry in strat_entry()) {
            let serialized = serialize_entry(&entry).unwrap();
            let deserialized: Entry = deserialize_entry(&serialized).unwrap();
            prop_assert_eq!(&entry, &deserialized);
        }

        #[test]
        fn roundtrip_multi(entries in strat_entries()) {
            let serialized = serialize_entry_multi(&entries).unwrap();
            let deserialized: Vec<Entry> = deserialize_entry_multi(&serialized).unwrap();
            prop_assert_eq!(entries, deserialized);
        }
    }
}
