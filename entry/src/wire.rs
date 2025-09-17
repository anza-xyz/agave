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
    solana_message::{
        legacy::Message as LegacyMessage,
        v0::{Message as V0Message, MessageAddressTableLookup},
        MessageHeader, VersionedMessage, MESSAGE_VERSION_PREFIX,
    },
    solana_short_vec::decode_shortu16_len,
    solana_transaction::{versioned::VersionedTransaction, CompiledInstruction},
    std::{marker::PhantomData, mem::MaybeUninit, ptr},
};

/// Behavior for Bincode's endianness configuration.
///
/// Bincode hides all its endianness functionality from exported modules, so
/// this provides a small utility that can be used to access the configured
/// endianness on the [`bincode::Options`] trait.
///
/// Because the wire format is fixed, we don't necessarily need this abstraction;
/// we could simply follow the default little endian implementation. That being
/// said, this also prevents anyone from trying to serialize / deserialize
/// with a different endianness at compile time, which is a safety precaution.
pub trait BincodeEndian {
    fn u64(ptr: &mut u64);
}

// Wire format uses little endian, so we only provide a little endian implementation.
// This prevents serialization of [`Entry`] with BE encoding.
impl BincodeEndian for bincode::config::LittleEndian {
    #[inline(always)]
    fn u64(_ptr: &mut u64) {
        #[cfg(not(target_endian = "little"))]
        {
            *_ptr = _ptr.swap_bytes();
        }
    }
}

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
pub trait SeqLen<O> {
    fn get_len(reader: &mut Reader<O>) -> bincode::Result<usize>;
    fn encode_len(writer: &mut Writer<O>, len: usize) -> bincode::Result<()>;
}

// Wire format uses bincode's default fixint length encoding for non-solana-sdk sequences.
impl<O> SeqLen<O> for bincode::config::FixintEncoding
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
    #[inline(always)]
    fn get_len(reader: &mut Reader<O>) -> bincode::Result<usize> {
        // bincode fixint encodes length as a literal u64
        Ok(reader.get_u64()? as usize)
    }

    #[inline(always)]
    fn encode_len(writer: &mut Writer<O>, len: usize) -> bincode::Result<()> {
        // bincode fixint encodes length as a literal u64
        writer.write_u64(len as u64)?;
        Ok(())
    }
}

/// Helper that extracts the `bincode::Options::IntEncoding` type from the given
/// [`bincode::Options`] type.
struct BincodeLen<O>(PhantomData<O>);

impl<O> SeqLen<O> for BincodeLen<O>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    #[inline(always)]
    fn get_len(reader: &mut Reader<O>) -> bincode::Result<usize> {
        <O::IntEncoding as SeqLen<O>>::get_len(reader)
    }

    #[inline(always)]
    fn encode_len(writer: &mut Writer<O>, len: usize) -> bincode::Result<()> {
        <O::IntEncoding as SeqLen<O>>::encode_len(writer, len)
    }
}

/// Helper that can be passed to [`Reader::read_seq`] and [`Writer::write_seq`] to
/// specify that the sequence's length should be encoded according to bincode's
/// length encoding.
#[inline(always)]
fn len_bincode<O>() -> BincodeLen<O> {
    BincodeLen(PhantomData)
}

struct ShortU16Len<O>(PhantomData<O>);

impl<O> SeqLen<O> for ShortU16Len<O> {
    #[inline(always)]
    fn get_len(cursor: &mut Reader<O>) -> bincode::Result<usize> {
        let (len, read) = decode_shortu16_len(&cursor.cursor[cursor.pos..])
            .map_err(|_| Box::new(bincode::ErrorKind::Custom("Invalid short u16".to_string())))?;
        cursor.pos += read;
        Ok(len)
    }

    #[inline(always)]
    fn encode_len(writer: &mut Writer<O>, len: usize) -> bincode::Result<()> {
        if len > u16::MAX as usize {
            return Err(Box::new(bincode::ErrorKind::Custom(
                "Invalid short u16".to_string(),
            )));
        }

        let rem = len as u16;

        let needed = 1 + (rem >= 0x80) as usize + (rem >= 0x4000) as usize;
        let cap = writer.buffer.capacity();
        let free = cap.wrapping_sub(writer.pos);
        if free < needed {
            return Err(bincode::ErrorKind::SizeLimit.into());
        }

        unsafe {
            let dst = writer.buffer.as_mut_ptr().add(writer.pos);

            match needed {
                1 => std::ptr::write(dst, rem as u8),
                2 => {
                    std::ptr::write(dst, ((rem & 0x7f) as u8) | 0x80);
                    std::ptr::write(dst.add(1), (rem >> 7) as u8);
                }
                3 => {
                    std::ptr::write(dst, ((rem & 0x7f) as u8) | 0x80);
                    std::ptr::write(dst.add(1), (((rem >> 7) & 0x7f) as u8) | 0x80);
                    std::ptr::write(dst.add(2), (rem >> 14) as u8);
                }
                _ => unreachable!(),
            }
        }

        writer.set_pos(writer.pos + needed);

        Ok(())
    }
}

/// Helper that can be passed to [`Reader::read_seq`] and [`Writer::write_seq`] to
/// specify that the sequence's length should be encoded according to the
/// [`solana_short_vec`] encoding.
#[inline(always)]
fn len_short_u16<O>() -> ShortU16Len<O> {
    ShortU16Len(PhantomData)
}

pub struct Reader<'a, O> {
    cursor: &'a [u8],
    pos: usize,
    _phantom: PhantomData<O>,
}

impl<'a, O> Reader<'a, O> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: bytes,
            pos: 0,
            _phantom: PhantomData,
        }
    }

    /// Copy exactly `len` bytes from the cursor into `buf`.
    #[inline]
    fn read_exact(&mut self, buf: *mut u8, len: usize) -> bincode::Result<()> {
        if self.pos + len > self.cursor.len() {
            return Err(bincode::ErrorKind::SizeLimit.into());
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
        F: Fn(&mut Reader<'a, O>, *mut T) -> bincode::Result<()>,
        Len: SeqLen<O>,
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
        Len: SeqLen<O>,
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

impl<O> Reader<'_, O>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
    #[inline(always)]
    fn read_u64(&mut self, ptr: *mut u64) -> bincode::Result<()> {
        self.read_t(ptr)?;
        // SAFETY: ptr is initialized by read_t
        <O::Endian as BincodeEndian>::u64(unsafe { &mut *ptr });
        Ok(())
    }

    #[inline(always)]
    fn get_u64(&mut self) -> bincode::Result<u64> {
        let mut u64 = MaybeUninit::<u64>::uninit();
        self.read_u64(u64.as_mut_ptr())?;
        // SAFETY: u64 is initialized by read_u64
        Ok(unsafe { u64.assume_init() })
    }
}

fn de_compiled_instruction<O>(
    cursor: &mut Reader<'_, O>,
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
    cursor.read_byte_seq(accounts_ptr, len_short_u16())?;
    cursor.read_byte_seq(data_ptr, len_short_u16())?;
    Ok(())
}

fn de_legacy_message<O>(
    cursor: &mut Reader<'_, O>,
    ptr: *mut LegacyMessage,
) -> bincode::Result<()> {
    let (account_keys_ptr, recent_blockhash_ptr, instructions_ptr) = unsafe {
        (
            &raw mut (*ptr).account_keys,
            &raw mut (*ptr).recent_blockhash,
            &raw mut (*ptr).instructions,
        )
    };
    cursor.read_byte_seq(account_keys_ptr, len_short_u16())?;
    cursor.read_t(recent_blockhash_ptr)?;
    cursor.read_seq(instructions_ptr, len_short_u16(), de_compiled_instruction)?;
    Ok(())
}

fn de_address_table_lookup<O>(
    cursor: &mut Reader<'_, O>,
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
    cursor.read_byte_seq(writable_indexes_ptr, len_short_u16())?;
    cursor.read_byte_seq(readonly_indexes_ptr, len_short_u16())?;
    Ok(())
}

fn de_v0_message<O>(cursor: &mut Reader<'_, O>, ptr: *mut V0Message) -> bincode::Result<()> {
    let (account_keys_ptr, recent_blockhash_ptr, instructions_ptr, address_table_lookups_ptr) = unsafe {
        (
            &raw mut (*ptr).account_keys,
            &raw mut (*ptr).recent_blockhash,
            &raw mut (*ptr).instructions,
            &raw mut (*ptr).address_table_lookups,
        )
    };
    cursor.read_byte_seq(account_keys_ptr, len_short_u16())?;
    cursor.read_t(recent_blockhash_ptr)?;
    cursor.read_seq(instructions_ptr, len_short_u16(), de_compiled_instruction)?;
    cursor.read_seq(
        address_table_lookups_ptr,
        len_short_u16(),
        de_address_table_lookup,
    )?;
    Ok(())
}

/// See [`solana_message::VersionedMessage`] for more details.
fn de_versioned_message<O>(
    cursor: &mut Reader<'_, O>,
    ptr: *mut VersionedMessage,
) -> bincode::Result<()> {
    // From `solana_message`:
    //
    // If the first bit is set, the remaining 7 bits will be used to determine
    // which message version is serialized starting from version `0`. If the first
    // is bit is not set, all bytes are used to encode the legacy `Message`
    // format.
    let variant = cursor.get_t::<u8>()?;

    if variant & MESSAGE_VERSION_PREFIX != 0 {
        let version = variant & !MESSAGE_VERSION_PREFIX;
        match version {
            0 => {
                let mut msg = MaybeUninit::<V0Message>::uninit();
                let msg_ptr = msg.as_mut_ptr();
                let num_required_signatures_ptr =
                    unsafe { &raw mut (*msg_ptr).header.num_required_signatures };
                // header is serialized as 3 contiguous bytes, we can grab them all in one go.
                cursor.read_t(num_required_signatures_ptr as *mut [u8; 3])?;
                de_v0_message(cursor, msg_ptr)?;
                unsafe {
                    ptr::write(ptr, VersionedMessage::V0(msg.assume_init()));
                }
            }
            127 => {
                return Err(bincode::ErrorKind::InvalidTagEncoding(127).into());
            }
            _ => {
                return Err(bincode::ErrorKind::InvalidTagEncoding(version as usize).into());
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
        // read the next 2 contiguous bytes
        cursor.read_t(num_readonly_signed_accounts_ptr as *mut [u8; 2])?;
        de_legacy_message(cursor, msg_ptr)?;
        unsafe {
            ptr::write(ptr, VersionedMessage::Legacy(msg.assume_init()));
        }
    }

    Ok(())
}

fn de_versioned_transaction<O>(
    cursor: &mut Reader<'_, O>,
    ptr: *mut VersionedTransaction,
) -> bincode::Result<()> {
    let (signatures_ptr, message_ptr) =
        unsafe { (&raw mut (*ptr).signatures, &raw mut (*ptr).message) };
    cursor.read_byte_seq(signatures_ptr, len_short_u16())?;
    de_versioned_message(cursor, message_ptr)?;
    Ok(())
}

#[inline(always)]
fn de_entry<O>(cursor: &mut Reader<'_, O>, ptr: *mut Entry) -> bincode::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    let (num_hashes_ptr, hash_ptr, txs_ptr) = unsafe {
        (
            &raw mut (*ptr).num_hashes,
            &raw mut (*ptr).hash,
            &raw mut (*ptr).transactions,
        )
    };

    cursor.read_u64(num_hashes_ptr)?;
    cursor.read_t(hash_ptr)?;
    cursor.read_seq(txs_ptr, len_bincode(), de_versioned_transaction)?;

    Ok(())
}

pub trait Deserialize {
    fn deserialize<O>(bytes: &[u8], options: O) -> bincode::Result<Self>
    where
        Self: Sized,
        O: bincode::Options,
        O::Endian: BincodeEndian,
        O::IntEncoding: SeqLen<O>;
}

impl Deserialize for Entry {
    fn deserialize<O>(bytes: &[u8], options: O) -> bincode::Result<Self>
    where
        O: bincode::Options,
        O::Endian: BincodeEndian,
        O::IntEncoding: SeqLen<O>,
    {
        deserialize_entry(bytes, options)
    }
}

impl Deserialize for Vec<Entry> {
    fn deserialize<O>(bytes: &[u8], options: O) -> bincode::Result<Self>
    where
        O: bincode::Options,
        O::Endian: BincodeEndian,
        O::IntEncoding: SeqLen<O>,
    {
        deserialize_entry_multi(bytes, options)
    }
}

#[inline(always)]
pub fn deserialize_entry<O>(bytes: &[u8], _options: O) -> bincode::Result<Entry>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    let mut cursor: Reader<'_, O> = Reader::new(bytes);
    let mut entry = MaybeUninit::<Entry>::uninit();
    let entry_ptr = entry.as_mut_ptr();
    de_entry(&mut cursor, entry_ptr)?;
    Ok(unsafe { entry.assume_init() })
}

#[inline(always)]
pub fn deserialize_entry_multi<O>(slice: &[u8], _options: O) -> bincode::Result<Vec<Entry>>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    let mut cursor: Reader<'_, O> = Reader::new(slice);
    let mut entries = MaybeUninit::<Vec<Entry>>::uninit();
    cursor.read_seq(entries.as_mut_ptr(), len_bincode(), de_entry)?;
    Ok(unsafe { entries.assume_init() })
}

pub struct Writer<'a, O> {
    buffer: &'a mut Vec<u8>,
    pos: usize,
    _phantom: PhantomData<O>,
}

impl<'a, O> Writer<'a, O> {
    fn new(buffer: &'a mut Vec<u8>) -> Self {
        Self {
            buffer,
            pos: 0,
            _phantom: PhantomData,
        }
    }

    #[inline(always)]
    fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
        unsafe {
            self.buffer.set_len(pos);
        }
    }

    /// Write exactly `len` bytes from `buf` into the internal buffer.
    #[inline(always)]
    fn write(&mut self, buf: *const u8, len: usize) -> bincode::Result<()> {
        if self.pos + len > self.buffer.capacity() {
            return Err(bincode::ErrorKind::SizeLimit.into());
        }
        unsafe {
            ptr::copy_nonoverlapping(buf, self.buffer.as_mut_ptr().add(self.pos), len);
        }
        self.set_pos(self.pos + len);
        Ok(())
    }

    /// Write T into the internal buffer.
    #[inline(always)]
    fn write_t<T>(&mut self, value: &T) -> bincode::Result<()> {
        self.write(value as *const T as *const u8, size_of::<T>())
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
        F: Fn(&mut Writer<'a, O>, &T) -> bincode::Result<()>,
        Len: SeqLen<O>,
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
        Len: SeqLen<O>,
    {
        Len::encode_len(self, value.len())?;
        self.write(value.as_ptr() as *const u8, size_of_val(value))
    }
}

impl<O> Writer<'_, O>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
    #[inline(always)]
    fn write_u64(&mut self, mut value: u64) -> bincode::Result<()> {
        <O::Endian as BincodeEndian>::u64(&mut value);
        self.write_t(&value)?;
        Ok(())
    }
}

fn se_compiled_instruction<O>(
    writer: &mut Writer<'_, O>,
    value: &CompiledInstruction,
) -> bincode::Result<()> {
    writer.write_t(&value.program_id_index)?;
    writer.write_byte_seq(&value.accounts, len_short_u16())?;
    writer.write_byte_seq(&value.data, len_short_u16())?;
    Ok(())
}

#[inline(always)]
fn se_header<O>(writer: &mut Writer<'_, O>, value: &MessageHeader) -> bincode::Result<()> {
    writer.write_bytes(&[
        value.num_required_signatures,
        value.num_readonly_signed_accounts,
        value.num_readonly_unsigned_accounts,
    ])?;
    Ok(())
}

fn se_legacy_message<O>(writer: &mut Writer<'_, O>, value: &LegacyMessage) -> bincode::Result<()> {
    se_header(writer, &value.header)?;
    writer.write_byte_seq(&value.account_keys, len_short_u16())?;
    writer.write_bytes(value.recent_blockhash.as_ref())?;
    writer.write_seq(
        &value.instructions,
        len_short_u16(),
        se_compiled_instruction,
    )?;
    Ok(())
}

fn se_address_table_lookup<O>(
    writer: &mut Writer<'_, O>,
    value: &MessageAddressTableLookup,
) -> bincode::Result<()> {
    writer.write_bytes(value.account_key.as_ref())?;
    writer.write_byte_seq(&value.writable_indexes, len_short_u16())?;
    writer.write_byte_seq(&value.readonly_indexes, len_short_u16())?;
    Ok(())
}

fn se_v0_message<O>(writer: &mut Writer<'_, O>, value: &V0Message) -> bincode::Result<()> {
    writer.write_t(&MESSAGE_VERSION_PREFIX)?;
    se_header(writer, &value.header)?;
    writer.write_byte_seq(&value.account_keys, len_short_u16())?;
    writer.write_bytes(value.recent_blockhash.as_ref())?;
    writer.write_seq(
        &value.instructions,
        len_short_u16(),
        se_compiled_instruction,
    )?;
    writer.write_seq(
        &value.address_table_lookups,
        len_short_u16(),
        se_address_table_lookup,
    )?;
    Ok(())
}

#[inline(always)]
fn se_versioned_message<O>(
    writer: &mut Writer<'_, O>,
    value: &VersionedMessage,
) -> bincode::Result<()> {
    match value {
        VersionedMessage::Legacy(message) => se_legacy_message(writer, message),
        VersionedMessage::V0(message) => se_v0_message(writer, message),
    }
}

fn se_versioned_transaction<O>(
    writer: &mut Writer<'_, O>,
    value: &VersionedTransaction,
) -> bincode::Result<()> {
    writer.write_byte_seq(&value.signatures, len_short_u16())?;
    se_versioned_message(writer, &value.message)?;
    Ok(())
}

fn se_entry<O>(writer: &mut Writer<'_, O>, value: &Entry) -> bincode::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    writer.write_u64(value.num_hashes)?;
    writer.write_bytes(value.hash.as_ref())?;
    writer.write_seq(&value.transactions, len_bincode(), se_versioned_transaction)?;
    Ok(())
}

pub trait Serialize {
    fn serialize<O>(&self, options: O) -> bincode::Result<Vec<u8>>
    where
        O: bincode::Options,
        O::Endian: BincodeEndian,
        O::IntEncoding: SeqLen<O>;
}

impl Serialize for Entry {
    fn serialize<O>(&self, options: O) -> bincode::Result<Vec<u8>>
    where
        O: bincode::Options,
        O::Endian: BincodeEndian,
        O::IntEncoding: SeqLen<O>,
    {
        serialize_entry(self, options)
    }
}

impl Serialize for &[Entry] {
    fn serialize<O>(&self, options: O) -> bincode::Result<Vec<u8>>
    where
        O: bincode::Options,
        O::Endian: BincodeEndian,
        O::IntEncoding: SeqLen<O>,
    {
        serialize_entry_multi(self, options)
    }
}

pub fn serialize_entry<O>(entry: &Entry, options: O) -> bincode::Result<Vec<u8>>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    let size = options.serialized_size(entry)?;
    let mut buffer = Vec::with_capacity(size as usize);
    let mut writer: Writer<'_, O> = Writer::new(&mut buffer);
    se_entry(&mut writer, entry)?;
    Ok(buffer)
}

pub fn serialize_entry_multi<O>(entries: &[Entry], options: O) -> bincode::Result<Vec<u8>>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    let size = options.serialized_size(entries)?;
    let mut buffer = Vec::with_capacity(size as usize);
    let mut writer: Writer<'_, O> = Writer::new(&mut buffer);
    writer.write_seq(entries, len_bincode(), se_entry)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        bincode::Options,
        proptest::prelude::*,
        solana_address::{Address, ADDRESS_BYTES},
        solana_hash::{Hash, HASH_BYTES},
        solana_message::MessageHeader,
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

    proptest! {
        #[test]
        fn de_equivalence(entry in strat_entry()) {
            let options = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes();
            let serialized = options.serialize(&entry).unwrap();
            let deserialized: Entry = deserialize_entry(&serialized, options).unwrap();
            prop_assert_eq!(entry, deserialized);
        }

        #[test]
        fn de_multi_equivalence(entries in strat_entries()) {
            let options = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes();
            let serialized = options.serialize(&entries).unwrap();
            let deserialized: Vec<Entry> = deserialize_entry_multi(&serialized, options).unwrap();
            prop_assert_eq!(entries, deserialized);
        }

        #[test]
        fn ser_equivalence(entry in strat_entry()) {
            let options = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes();
            let serialized = serialize_entry(&entry, options).unwrap();
            prop_assert_eq!(serialized, options.serialize(&entry).unwrap());
        }

        #[test]
        fn ser_multi_equivalence(entries in strat_entries()) {
            let options = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes();
            let serialized = serialize_entry_multi(&entries, options).unwrap();
            prop_assert_eq!(serialized, options.serialize(&entries).unwrap());
        }

        #[test]
        fn roundtrip(entry in strat_entry()) {
            let options = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes();
            let serialized = serialize_entry(&entry, options).unwrap();
            let deserialized: Entry = deserialize_entry(&serialized, options).unwrap();
            prop_assert_eq!(&entry, &deserialized);
        }

        #[test]
        fn roundtrip_multi(entries in strat_entries()) {
            let options = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes();
            let serialized = serialize_entry_multi(&entries, options).unwrap();
            let deserialized: Vec<Entry> = deserialize_entry_multi(&serialized, options).unwrap();
            prop_assert_eq!(entries, deserialized);
        }
    }
}
