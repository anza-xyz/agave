use {
    crate::entry::Entry,
    solana_message::{
        legacy::Message as LegacyMessage,
        v0::{Message as V0Message, MessageAddressTableLookup},
        VersionedMessage, MESSAGE_VERSION_PREFIX,
    },
    solana_short_vec::decode_shortu16_len,
    solana_transaction::{versioned::VersionedTransaction, CompiledInstruction},
    std::{
        io::{self},
        marker::PhantomData,
        mem::MaybeUninit,
        ptr,
    },
};

pub trait BincodeEndian {
    fn u64(ptr: &mut u64);
}

impl BincodeEndian for bincode::config::LittleEndian {
    #[inline(always)]
    fn u64(_ptr: &mut u64) {
        #[cfg(not(target_endian = "little"))]
        {
            *_ptr = _ptr.swap_bytes();
        }
    }
}

impl BincodeEndian for bincode::config::BigEndian {
    #[inline(always)]
    fn u64(_ptr: &mut u64) {
        #[cfg(not(target_endian = "big"))]
        {
            *_ptr = _ptr.swap_bytes();
        }
    }
}

pub trait SeqLen<O> {
    fn get_len(cursor: &mut Cursor<O>) -> io::Result<usize>;
}

impl<O> SeqLen<O> for bincode::config::FixintEncoding
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
    #[inline(always)]
    fn get_len(cursor: &mut Cursor<O>) -> io::Result<usize> {
        // bincode fixint encoding always encodes length as a literal u64 with configured endianness
        Ok(cursor.get_u64()? as usize)
    }
}

struct BincodeLen<O>(PhantomData<O>);

impl<O> SeqLen<O> for BincodeLen<O>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    #[inline(always)]
    fn get_len(cursor: &mut Cursor<O>) -> io::Result<usize> {
        <O::IntEncoding as SeqLen<O>>::get_len(cursor)
    }
}

#[inline(always)]
fn len_bincode<O>() -> BincodeLen<O> {
    BincodeLen(PhantomData)
}

struct ShortU16Len<O>(PhantomData<O>);

impl<O> SeqLen<O> for ShortU16Len<O> {
    #[inline(always)]
    fn get_len(cursor: &mut Cursor<O>) -> io::Result<usize> {
        let (len, read) = decode_shortu16_len(&cursor.cursor[cursor.pos..])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid short u16"))?;
        cursor.pos += read;
        Ok(len)
    }
}

#[inline(always)]
fn len_short_u16<O>() -> ShortU16Len<O> {
    ShortU16Len(PhantomData)
}

pub struct Cursor<'a, O> {
    cursor: &'a [u8],
    pos: usize,
    _options: O,
}

impl<'a, O> Cursor<'a, O> {
    #[inline(always)]
    fn new(bytes: &'a [u8], options: O) -> Self {
        Self {
            cursor: bytes,
            pos: 0,
            _options: options,
        }
    }

    #[inline]
    fn read_exact(&mut self, buf: *mut u8, len: usize) -> io::Result<()> {
        if self.pos + len > self.cursor.len() {
            return Err(io::Error::new(std::io::ErrorKind::UnexpectedEof, "EOF"));
        }
        unsafe {
            ptr::copy_nonoverlapping(self.cursor.as_ptr().add(self.pos), buf, len);
        }
        self.pos += len;
        Ok(())
    }

    #[inline(always)]
    fn read_t<T>(&mut self, ptr: *mut T) -> io::Result<()> {
        self.read_exact(ptr as *mut u8, size_of::<T>())
    }

    #[inline(always)]
    fn get_t<T>(&mut self) -> io::Result<T> {
        let mut t = MaybeUninit::<T>::uninit();
        self.read_t(t.as_mut_ptr())?;
        Ok(unsafe { t.assume_init() })
    }

    fn read_seq<T, F, Len>(&mut self, ptr: *mut Vec<T>, _len: Len, parse_t: F) -> io::Result<()>
    where
        F: Fn(&mut Cursor<'a, O>, *mut T) -> io::Result<()>,
        Len: SeqLen<O>,
    {
        let len = Len::get_len(self)?;
        let mut vec: Vec<T> = Vec::with_capacity(len);
        let mut vec_ptr = vec.spare_capacity_mut().as_mut_ptr();
        for i in 0..len {
            parse_t(self, vec_ptr as *mut T)?;
            unsafe {
                vec_ptr = vec_ptr.add(1);
                vec.set_len(i + 1);
            }
        }
        unsafe {
            ptr::write(ptr, vec);
        }
        Ok(())
    }

    fn read_byte_seq<T, Len>(&mut self, ptr: *mut Vec<T>, _len: Len) -> io::Result<()>
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

impl<O> Cursor<'_, O>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
    #[inline(always)]
    fn read_u64(&mut self, ptr: *mut u64) -> io::Result<()> {
        self.read_t(ptr)?;
        // SAFETY: ptr is initialized by read_exact
        <O::Endian as BincodeEndian>::u64(unsafe { &mut *ptr });
        Ok(())
    }

    #[inline(always)]
    fn get_u64(&mut self) -> io::Result<u64> {
        let mut u64 = MaybeUninit::<u64>::uninit();
        self.read_u64(u64.as_mut_ptr())?;
        Ok(unsafe { u64.assume_init() })
    }
}

fn de_compiled_instruction<O>(
    cursor: &mut Cursor<'_, O>,
    ptr: *mut CompiledInstruction,
) -> io::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
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

fn de_legacy_message<O>(cursor: &mut Cursor<'_, O>, ptr: *mut LegacyMessage) -> io::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
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
    cursor: &mut Cursor<'_, O>,
    ptr: *mut MessageAddressTableLookup,
) -> io::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
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

fn de_v0_message<O>(cursor: &mut Cursor<'_, O>, ptr: *mut V0Message) -> io::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
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

fn de_versioned_message<O>(cursor: &mut Cursor<'_, O>, ptr: *mut VersionedMessage) -> io::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
    let variant = cursor.get_t::<u8>()?;

    if variant & MESSAGE_VERSION_PREFIX != 0 {
        let version = variant & !MESSAGE_VERSION_PREFIX;
        match version {
            0 => {
                let mut msg = MaybeUninit::<V0Message>::uninit();
                let msg_ptr = msg.as_mut_ptr();
                let (
                    num_required_signatures_ptr,
                    num_readonly_signed_accounts_ptr,
                    num_readonly_unsigned_accounts_ptr,
                ) = unsafe {
                    (
                        &raw mut (*msg_ptr).header.num_required_signatures,
                        &raw mut (*msg_ptr).header.num_readonly_signed_accounts,
                        &raw mut (*msg_ptr).header.num_readonly_unsigned_accounts,
                    )
                };
                cursor.read_t(num_required_signatures_ptr)?;
                cursor.read_t(num_readonly_signed_accounts_ptr)?;
                cursor.read_t(num_readonly_unsigned_accounts_ptr)?;
                de_v0_message(cursor, msg_ptr)?;
                unsafe {
                    ptr::write(ptr, VersionedMessage::V0(msg.assume_init()));
                }
            }
            127 => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "off-chain messages are not accepted",
                ));
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid message version",
                ));
            }
        }
    } else {
        let mut msg = MaybeUninit::<LegacyMessage>::uninit();
        let msg_ptr = msg.as_mut_ptr();
        let (
            num_required_signatures_ptr,
            num_readonly_signed_accounts_ptr,
            num_readonly_unsigned_accounts_ptr,
        ) = unsafe {
            (
                &raw mut (*msg_ptr).header.num_required_signatures,
                &raw mut (*msg_ptr).header.num_readonly_signed_accounts,
                &raw mut (*msg_ptr).header.num_readonly_unsigned_accounts,
            )
        };
        unsafe {
            ptr::write(num_required_signatures_ptr, variant);
        }
        cursor.read_t(num_readonly_signed_accounts_ptr)?;
        cursor.read_t(num_readonly_unsigned_accounts_ptr)?;
        de_legacy_message(cursor, msg_ptr)?;
        unsafe {
            ptr::write(ptr, VersionedMessage::Legacy(msg.assume_init()));
        }
    }

    Ok(())
}

fn de_versioned_transaction<O>(
    cursor: &mut Cursor<'_, O>,
    ptr: *mut VersionedTransaction,
) -> io::Result<()>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
{
    let (signatures_ptr, message_ptr) =
        unsafe { (&raw mut (*ptr).signatures, &raw mut (*ptr).message) };
    cursor.read_byte_seq(signatures_ptr, len_short_u16())?;
    de_versioned_message(cursor, message_ptr)?;
    Ok(())
}

#[inline(always)]
pub fn deserialize_entry<O>(bytes: &[u8], options: O) -> bincode::Result<Entry>
where
    O: bincode::Options,
    O::Endian: BincodeEndian,
    O::IntEncoding: SeqLen<O>,
{
    let mut cursor = Cursor::new(bytes, options);
    let mut entry = MaybeUninit::<Entry>::uninit();
    let entry_ptr = entry.as_mut_ptr();
    let (num_hashes_ptr, hash_ptr, txs_ptr) = unsafe {
        (
            &raw mut (*entry_ptr).num_hashes,
            &raw mut (*entry_ptr).hash,
            &raw mut (*entry_ptr).transactions,
        )
    };

    cursor.read_u64(num_hashes_ptr)?;
    cursor.read_t(hash_ptr)?;
    cursor.read_seq(txs_ptr, len_bincode(), de_versioned_transaction)?;

    Ok(unsafe { entry.assume_init() })
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

    proptest! {
        #[test]
        fn serde_roundtrip(entry in strat_entry()) {
            let options = bincode::DefaultOptions::new()
                .with_fixint_encoding();
            let serialized = options.serialize(&entry).unwrap();
            let deserialized: Entry = deserialize_entry(&serialized, options).unwrap();
            prop_assert_eq!(entry, deserialized);
        }
    }
}
