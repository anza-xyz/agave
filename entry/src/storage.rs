use {
    serde::{
        de::{self, DeserializeSeed, Deserializer, Error, SeqAccess, VariantAccess, Visitor},
        ser::{SerializeSeq, SerializeTuple},
        Deserialize, Serialize,
    },
    serde_bytes::{ByteBuf, Bytes},
    solana_address::ADDRESS_BYTES,
    solana_hash::HASH_BYTES,
    solana_message::{
        legacy,
        v0::{self, MessageAddressTableLookup},
        MessageHeader, VersionedMessage,
    },
    solana_signature::SIGNATURE_BYTES,
    solana_transaction::{versioned::VersionedTransaction, CompiledInstruction},
    std::{
        fmt,
        marker::PhantomData,
        mem::{transmute, MaybeUninit},
        ptr::{self},
        result::Result,
        slice,
    },
};

/// Serialize a sequence of fixed length byte arrays as a single byte slice.
///
/// This avoids encoding a sequence of byte arrays individually, and instead
/// encodes the entire sequence in one pass.
///
/// Should be coupled with [`FixedByteVecSeed`] for deserialization.
///
/// Note: we're using the `From<[u8; N]>` constraint as a convenience
/// because `Hash`, `Signature`, and `Address` all implement it.
pub struct FixedByteVecSer<'a, const N: usize, T: From<[u8; N]>>(pub &'a [T]);
impl<T, const N: usize> Serialize for FixedByteVecSer<'_, N, T>
where
    T: From<[u8; N]>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        debug_assert_eq!(size_of::<T>(), N);
        debug_assert_eq!(align_of::<T>(), align_of::<[u8; N]>());

        let ptr = self.0.as_ptr() as *const u8;
        let slice = unsafe { slice::from_raw_parts(ptr, self.0.len() * N) };
        serializer.serialize_bytes(slice)
    }
}

struct MessageHeaderSeed(*mut MessageHeader);
impl<'de> DeserializeSeed<'de> for MessageHeaderSeed {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        unsafe {
            ptr::write(self.0, <MessageHeader>::deserialize(deserializer)?);
        }

        Ok(())
    }
}

struct CompiledInstructionSer<'a>(&'a CompiledInstruction);
impl Serialize for CompiledInstructionSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_tuple(3)?;
        s.serialize_element(&self.0.program_id_index)?;
        s.serialize_element(&Bytes::new(&self.0.accounts))?;
        s.serialize_element(&Bytes::new(&self.0.data))?;
        s.end()
    }
}

struct CompiledInstructionSeed(*mut CompiledInstruction);
impl<'de> DeserializeSeed<'de> for CompiledInstructionSeed {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitCompiledInstruction {
            dst: *mut CompiledInstruction,
        }

        impl<'de> Visitor<'de> for VisitCompiledInstruction {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a CompiledInstruction")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let (program_id_index_ptr, accounts_ptr, data_ptr) = unsafe {
                    (
                        &raw mut ((*self.dst).program_id_index),
                        &raw mut ((*self.dst).accounts),
                        &raw mut ((*self.dst).data),
                    )
                };

                let program_id_index = seq
                    .next_element::<u8>()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let accounts = seq
                    .next_element::<ByteBuf>()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?
                    .into_vec();
                let data = seq
                    .next_element::<ByteBuf>()?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?
                    .into_vec();

                unsafe {
                    ptr::write(program_id_index_ptr, program_id_index);
                    ptr::write(accounts_ptr, accounts);
                    ptr::write(data_ptr, data);
                }
                Ok(())
            }
        }

        deserializer.deserialize_tuple(3, VisitCompiledInstruction { dst: self.0 })
    }
}

struct CompiledInstructionsSer<'a>(&'a [CompiledInstruction]);
impl Serialize for CompiledInstructionsSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_seq(Some(self.0.len()))?;
        for instruction in self.0 {
            s.serialize_element(&CompiledInstructionSer(instruction))?;
        }
        s.end()
    }
}

struct LegacyMessageSer<'a>(&'a legacy::Message);
impl Serialize for LegacyMessageSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_tuple(4)?;
        s.serialize_element(&self.0.header)?;
        s.serialize_element(&FixedByteVecSer(&self.0.account_keys))?;
        s.serialize_element(&Bytes::new(self.0.recent_blockhash.as_bytes()))?;
        s.serialize_element(&CompiledInstructionsSer(&self.0.instructions))?;
        s.end()
    }
}

struct LegacyMessageSeed(*mut legacy::Message);
impl<'de> DeserializeSeed<'de> for LegacyMessageSeed {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitLegacyMessage {
            dst: *mut legacy::Message,
        }

        impl<'de> Visitor<'de> for VisitLegacyMessage {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a LegacyMessage")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let (header_ptr, account_keys_ptr, recent_blockhash_ptr, instructions_ptr) = unsafe {
                    (
                        &raw mut (*self.dst).header,
                        &raw mut (*self.dst).account_keys as *mut Vec<[u8; ADDRESS_BYTES]>,
                        &raw mut (*self.dst).recent_blockhash as *mut [u8; HASH_BYTES],
                        &raw mut (*self.dst).instructions,
                    )
                };

                seq.next_element_seed(MessageHeaderSeed(header_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;

                seq.next_element_seed(FixedByteVecSeed(account_keys_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;

                seq.next_element_seed(FixedByteSeed(recent_blockhash_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?;

                seq.next_element_seed(VecSeqSeed::new(instructions_ptr, CompiledInstructionSeed))?
                    .ok_or_else(|| A::Error::invalid_length(3, &self))?;

                Ok(())
            }
        }

        deserializer.deserialize_tuple(4, VisitLegacyMessage { dst: self.0 })
    }
}

struct AddressTableLookupSer<'a>(&'a MessageAddressTableLookup);
impl Serialize for AddressTableLookupSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_tuple(3)?;
        s.serialize_element(&Bytes::new(self.0.account_key.as_ref()))?;
        s.serialize_element(&Bytes::new(&self.0.writable_indexes))?;
        s.serialize_element(&Bytes::new(&self.0.readonly_indexes))?;
        s.end()
    }
}

struct AddressTableLookupSeed(*mut MessageAddressTableLookup);
impl<'de> DeserializeSeed<'de> for AddressTableLookupSeed {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitAddressTableLookup {
            dst: *mut MessageAddressTableLookup,
        }

        impl<'de> Visitor<'de> for VisitAddressTableLookup {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a MessageAddressTableLookup")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let (account_key_ptr, writable_indexes_ptr, readonly_indexes_ptr) = unsafe {
                    (
                        &raw mut (*self.dst).account_key as *mut [u8; ADDRESS_BYTES],
                        &raw mut (*self.dst).writable_indexes,
                        &raw mut (*self.dst).readonly_indexes,
                    )
                };

                seq.next_element_seed(FixedByteSeed(account_key_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;

                let writeable_indexes = seq
                    .next_element::<ByteBuf>()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?
                    .into_vec();
                let readonly_indexes = seq
                    .next_element::<ByteBuf>()?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?
                    .into_vec();

                unsafe {
                    ptr::write(writable_indexes_ptr, writeable_indexes);
                    ptr::write(readonly_indexes_ptr, readonly_indexes);
                }

                Ok(())
            }
        }

        deserializer.deserialize_tuple(3, VisitAddressTableLookup { dst: self.0 })
    }
}

struct AddressTableLookupsSer<'a>(&'a [MessageAddressTableLookup]);
impl Serialize for AddressTableLookupsSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_seq(Some(self.0.len()))?;
        for lookup in self.0 {
            s.serialize_element(&AddressTableLookupSer(lookup))?;
        }
        s.end()
    }
}

struct V0MessageSer<'a>(&'a v0::Message);
impl Serialize for V0MessageSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_tuple(5)?;
        s.serialize_element(&self.0.header)?;
        s.serialize_element(&FixedByteVecSer(&self.0.account_keys))?;
        s.serialize_element(&Bytes::new(self.0.recent_blockhash.as_bytes()))?;
        s.serialize_element(&CompiledInstructionsSer(&self.0.instructions))?;
        s.serialize_element(&AddressTableLookupsSer(&self.0.address_table_lookups))?;
        s.end()
    }
}

struct V0MessageSeed(*mut v0::Message);
impl<'de> DeserializeSeed<'de> for V0MessageSeed {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitV0Message {
            dst: *mut v0::Message,
        }

        impl<'de> Visitor<'de> for VisitV0Message {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a V0Message")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let (
                    header_ptr,
                    account_keys_ptr,
                    recent_blockhash_ptr,
                    instructions_ptr,
                    address_table_lookups_ptr,
                ) = unsafe {
                    (
                        &raw mut (*self.dst).header,
                        &raw mut (*self.dst).account_keys as *mut Vec<[u8; ADDRESS_BYTES]>,
                        &raw mut (*self.dst).recent_blockhash as *mut [u8; HASH_BYTES],
                        &raw mut (*self.dst).instructions,
                        &raw mut (*self.dst).address_table_lookups,
                    )
                };

                seq.next_element_seed(MessageHeaderSeed(header_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;

                seq.next_element_seed(FixedByteVecSeed(account_keys_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;

                seq.next_element_seed(FixedByteSeed(recent_blockhash_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?;

                seq.next_element_seed(VecSeqSeed::new(instructions_ptr, CompiledInstructionSeed))?
                    .ok_or_else(|| A::Error::invalid_length(3, &self))?;

                seq.next_element_seed(VecSeqSeed::new(
                    address_table_lookups_ptr,
                    AddressTableLookupSeed,
                ))?
                .ok_or_else(|| A::Error::invalid_length(4, &self))?;

                Ok(())
            }
        }

        deserializer.deserialize_tuple(5, VisitV0Message { dst: self.0 })
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Deserialize)]
enum VersionedMessageVariant {
    Legacy = 0,
    V0 = 1,
}

impl TryFrom<u32> for VersionedMessageVariant {
    type Error = ();

    #[inline(always)]
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(VersionedMessageVariant::Legacy),
            1 => Ok(VersionedMessageVariant::V0),
            _ => Err(()),
        }
    }
}

struct VersionedMessageSer<'a>(&'a VersionedMessage);
impl Serialize for VersionedMessageSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            VersionedMessage::Legacy(message) => serializer.serialize_newtype_variant(
                "VersionedMessage",
                VersionedMessageVariant::Legacy as u32,
                "Legacy",
                &LegacyMessageSer(message),
            ),
            VersionedMessage::V0(message) => serializer.serialize_newtype_variant(
                "VersionedMessage",
                VersionedMessageVariant::V0 as u32,
                "V0",
                &V0MessageSer(message),
            ),
        }
    }
}

struct VersionedMessageSeed(*mut VersionedMessage);
impl<'de> DeserializeSeed<'de> for VersionedMessageSeed {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitVersionedMessage {
            dst: *mut VersionedMessage,
        }

        impl<'de> Visitor<'de> for VisitVersionedMessage {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a VersionedMessage")
            }

            fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
            where
                A: de::EnumAccess<'de>,
            {
                let (variant, access) = data.variant::<VersionedMessageVariant>()?;

                match variant {
                    VersionedMessageVariant::Legacy => {
                        let mut msg = MaybeUninit::<legacy::Message>::uninit();
                        access.newtype_variant_seed(LegacyMessageSeed(msg.as_mut_ptr()))?;
                        let msg = unsafe { msg.assume_init() };
                        unsafe {
                            ptr::write(self.dst, VersionedMessage::Legacy(msg));
                        }
                    }
                    VersionedMessageVariant::V0 => {
                        let mut msg = MaybeUninit::<v0::Message>::uninit();
                        access.newtype_variant_seed(V0MessageSeed(msg.as_mut_ptr()))?;
                        let msg = unsafe { msg.assume_init() };
                        unsafe {
                            ptr::write(self.dst, VersionedMessage::V0(msg));
                        }
                    }
                }

                Ok(())
            }
        }

        deserializer.deserialize_enum(
            "VersionedMessage",
            &["Legacy", "V0"],
            VisitVersionedMessage { dst: self.0 },
        )
    }
}

struct VersionedTransactionsSer<'a>(&'a [VersionedTransaction]);
impl Serialize for VersionedTransactionsSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_seq(Some(self.0.len()))?;
        for tx in self.0 {
            s.serialize_element(&VersionedTransactionSer(tx))?;
        }
        s.end()
    }
}

struct VersionedTransactionSer<'a>(&'a VersionedTransaction);
impl Serialize for VersionedTransactionSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_tuple(2)?;
        s.serialize_element(&FixedByteVecSer(&self.0.signatures))?;
        s.serialize_element(&VersionedMessageSer(&self.0.message))?;
        s.end()
    }
}

struct VersionedTransactionSeed(*mut VersionedTransaction);
impl<'de> DeserializeSeed<'de> for VersionedTransactionSeed {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitVersionedTransaction {
            dst: *mut VersionedTransaction,
        }

        impl<'de> Visitor<'de> for VisitVersionedTransaction {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a VersionedTransaction")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let (sig_ptr, msg_ptr) = unsafe {
                    (
                        &raw mut (*self.dst).signatures as *mut Vec<[u8; SIGNATURE_BYTES]>,
                        &raw mut (*self.dst).message,
                    )
                };

                seq.next_element_seed(FixedByteVecSeed(sig_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;

                seq.next_element_seed(VersionedMessageSeed(msg_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;

                Ok(())
            }
        }

        deserializer.deserialize_tuple(2, VisitVersionedTransaction { dst: self.0 })
    }
}

#[repr(transparent)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Entry(pub crate::entry::Entry);

impl Entry {
    #[inline(always)]
    pub fn into_inner(self) -> crate::entry::Entry {
        unsafe { transmute(self) }
    }

    #[inline(always)]
    pub fn from_inner(entry: crate::entry::Entry) -> Self {
        unsafe { transmute(entry) }
    }
}

impl From<crate::entry::Entry> for Entry {
    #[inline(always)]
    fn from(value: crate::entry::Entry) -> Self {
        Entry::from_inner(value)
    }
}

impl From<Entry> for crate::entry::Entry {
    #[inline(always)]
    fn from(value: Entry) -> Self {
        value.into_inner()
    }
}

impl Serialize for Entry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let entry = &self.0;
        let mut s = serializer.serialize_tuple(3)?;
        s.serialize_element(&entry.num_hashes)?;
        s.serialize_element(&Bytes::new(entry.hash.as_bytes()))?;
        s.serialize_element(&VersionedTransactionsSer(&entry.transactions))?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for Entry {
    #[inline(always)]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RootVisitor;
        impl<'de> Visitor<'de> for RootVisitor {
            type Value = Entry;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("Entry")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entry = MaybeUninit::<Entry>::uninit();
                let entry_ptr = entry.as_mut_ptr();

                let (num_hashes_ptr, hash_ptr, txs_ptr) = unsafe {
                    (
                        &raw mut (*entry_ptr).0.num_hashes,
                        &raw mut (*entry_ptr).0.hash as *mut [u8; HASH_BYTES],
                        &raw mut (*entry_ptr).0.transactions,
                    )
                };

                let num_hashes = seq
                    .next_element::<u64>()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                unsafe {
                    ptr::write(num_hashes_ptr, num_hashes);
                }

                seq.next_element_seed(FixedByteSeed(hash_ptr))?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;

                seq.next_element_seed(VecSeqSeed::new(txs_ptr, VersionedTransactionSeed))?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?;

                Ok(unsafe { entry.assume_init() })
            }
        }

        deserializer.deserialize_tuple(3, RootVisitor)
    }
}

/// [`DeserializeSeed`] for a `Vec` of items.
///
/// Facilitates initializing a `Vec` in place.
///
/// This works by accepting a "`DeserializeSeed` generator" function
/// that is passed each uninitialized "slot" in the allocated memory.
/// This generator has the signature `Fn(&mut MaybeUninit<T>) -> Seed`, where `Seed`
/// is a [`DeserializeSeed`] for the item type.
///
/// # Example
/// ```
/// # use solana_entry::storage::{FixedByteSeed, VecSeqSeed};
/// # use serde::{Serialize, Deserialize, Deserializer, ser::{SerializeSeq, SerializeTuple}, de::{Visitor, SeqAccess, Error}};
/// # use serde_bytes::Bytes;
/// # use std::{fmt, mem::MaybeUninit};
/// # use rand::prelude::*;
/// # use core::array;
/// #[derive(Debug, PartialEq)]
/// #[repr(transparent)]
/// struct Hash(pub [u8; 32]);
///
/// #[derive(Debug, PartialEq)]
/// struct Message {
///     hashes: Vec<Hash>,
/// }
///
/// struct HashSer<'a>(&'a [Hash]);
/// impl Serialize for HashSer<'_> {
///     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
///     where
///         S: serde::Serializer,
///     {
///         let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
///         for hash in self.0 {
///             seq.serialize_element(&Bytes::new(&hash.0))?;
///         }
///         seq.end()
///     }
/// }
///
/// impl Serialize for Message {
///     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
///     where
///         S: serde::Serializer,
///     {
///         let mut seq = serializer.serialize_tuple(1)?;
///         seq.serialize_element(&HashSer(&self.hashes))?;
///         seq.end()
///     }
/// }
///
/// impl<'de> Deserialize<'de> for Message {
///     fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
///     where
///         D: Deserializer<'de>,
///     {
///         struct VisitMessage;
///
///         impl<'de> Visitor<'de> for VisitMessage {
///             type Value = Message;
///
///             fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
///                 formatter.write_str("a Message")
///             }
///
///             fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
///             where
///                 A: SeqAccess<'de>,
///             {
///                 let mut msg = MaybeUninit::<Message>::uninit();
///                 let msg_ptr = msg.as_mut_ptr();
///                 let hashes_ptr = unsafe { &raw mut (*msg_ptr).hashes } as *mut Vec<[u8; 32]>;
///                 // Write the hashes directly into a Vec
///                 seq.next_element_seed(VecSeqSeed::new(hashes_ptr, FixedByteSeed))?
///                     .ok_or_else(|| A::Error::invalid_length(0, &self))?;
///
///                 Ok(unsafe { msg.assume_init() })
///             }
///         }
///
///         deserializer.deserialize_tuple(1, VisitMessage)
///     }
/// }
///
/// fn main() {
///     let hashes = (0..10).map(|_| Hash(array::from_fn(|_| rand::random()))).collect();
///     let msg = Message { hashes };
///     let serialized = bincode::serialize(&msg).unwrap();
///     let deserialized = bincode::deserialize(&serialized).unwrap();
///     assert_eq!(msg, deserialized);
/// }
/// ```
pub struct VecSeqSeed<T, Seed, GenSeed> {
    dst: *mut Vec<T>,
    gen_seed: GenSeed,
    _marker: PhantomData<Seed>,
}

impl<T, Seed, GenSeed> VecSeqSeed<T, Seed, GenSeed> {
    pub fn new(dst: *mut Vec<T>, gen_seed: GenSeed) -> Self {
        Self {
            dst,
            gen_seed,
            _marker: PhantomData,
        }
    }
}

impl<'de, T, Seed, GenSeed> DeserializeSeed<'de> for VecSeqSeed<T, Seed, GenSeed>
where
    Seed: DeserializeSeed<'de>,
    GenSeed: FnMut(*mut T) -> Seed,
{
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitVecSeed<T, Seed, GenSeed> {
            dst: *mut Vec<T>,
            gen_seed: GenSeed,
            _marker: PhantomData<Seed>,
        }

        impl<'de, T, Seed, GenSeed> Visitor<'de> for VisitVecSeed<T, Seed, GenSeed>
        where
            Seed: DeserializeSeed<'de>,
            GenSeed: FnMut(*mut T) -> Seed,
        {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a sequence of items")
            }

            fn visit_seq<A>(mut self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                // Bincode guarantees length encoding.
                // https://git.sr.ht/~stygianentity/bincode/tree/92bd4a8852948bb51a23234b53e7347f94a10dfb/item/src/de/mod.rs#L293
                let seq_len = seq
                    .size_hint()
                    .ok_or_else(|| A::Error::custom("expected length encoded sequence"))?;

                if seq_len == 0 {
                    unsafe {
                        ptr::write(self.dst, Vec::new());
                    }
                    return Ok(());
                }

                let mut vec: Vec<T> = Vec::with_capacity(seq_len);
                let mut ptr = vec.spare_capacity_mut().as_mut_ptr();
                let mut visited_len = 0;

                while seq
                    .next_element_seed((self.gen_seed)(ptr as *mut T))?
                    .is_some()
                {
                    visited_len += 1;
                    unsafe {
                        // Set the len for drop safety.
                        vec.set_len(visited_len);
                        ptr = ptr.add(1);
                    }
                }

                if visited_len != seq_len {
                    return Err(A::Error::invalid_length(visited_len, &self));
                }

                unsafe {
                    ptr::write(self.dst, vec);
                }

                Ok(())
            }
        }

        deserializer.deserialize_seq(VisitVecSeed {
            dst: self.dst,
            gen_seed: self.gen_seed,
            _marker: self._marker,
        })
    }
}

/// [`DeserializeSeed`] for a sequence of fixed length byte arrays.
///
/// If your data is of the form `Vec<[u8; N]>`, prefer this over [`VecSeqSeed`],
/// as [`VecSeqSeed`] will iterate item-wise instead of copying the entire sequence
/// at once.
///
/// Should be coupled with [`FixedByteVecSer`] for serialization.
///
/// # Example
/// ```
/// # use solana_entry::storage::{FixedByteVecSeed, FixedByteVecSer};
/// # use serde::{Serialize, Deserialize, Deserializer, ser::{SerializeSeq, SerializeTuple}, de::{Visitor, SeqAccess, Error}};
/// # use serde_bytes::Bytes;
/// # use std::{fmt, mem::MaybeUninit};
/// # use rand::prelude::*;
/// # use core::array;
///
/// #[derive(Debug, PartialEq)]
/// struct Message {
///     hashes: Vec<solana_hash::Hash>,
/// }
///
/// impl Serialize for Message {
///     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
///     where
///         S: serde::Serializer,
///     {
///         let mut seq = serializer.serialize_tuple(1)?;
///         seq.serialize_element(&FixedByteVecSer(&self.hashes))?;
///         seq.end()
///     }
/// }
///
/// impl<'de> Deserialize<'de> for Message {
///     fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
///     where
///         D: Deserializer<'de>,
///     {
///         struct VisitMessage;
///
///         impl<'de> Visitor<'de> for VisitMessage {
///             type Value = Message;
///
///             fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
///                 formatter.write_str("a Message")
///             }
///
///             fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
///             where
///                 A: SeqAccess<'de>,
///             {
///                 let mut msg = MaybeUninit::<Message>::uninit();
///                 let msg_ptr = msg.as_mut_ptr();
///                 let hashes_ptr = unsafe { &raw mut (*msg_ptr).hashes } as *mut Vec<[u8; 32]>;
///                 // Write the hashes directly into a Vec
///                 seq.next_element_seed(FixedByteVecSeed(hashes_ptr))?
///                     .ok_or_else(|| A::Error::invalid_length(0, &self))?;
///
///                 Ok(unsafe { msg.assume_init() })
///             }
///         }
///
///         deserializer.deserialize_tuple(1, VisitMessage)
///     }
/// }
///
/// fn main() {
///     let hashes = (0..10).map(|_| solana_hash::Hash::from(array::from_fn(|_| rand::random()))).collect();
///     let msg = Message { hashes };
///     let serialized = bincode::serialize(&msg).unwrap();
///     let deserialized = bincode::deserialize(&serialized).unwrap();
///     assert_eq!(msg, deserialized);
/// }
/// ```
pub struct FixedByteVecSeed<const N: usize>(pub *mut Vec<[u8; N]>);

impl<'de, const N: usize> DeserializeSeed<'de> for FixedByteVecSeed<N> {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitFixedByteVecSeed<const N: usize> {
            dst: *mut Vec<[u8; N]>,
        }

        impl<const N: usize> Visitor<'_> for VisitFixedByteVecSeed<N> {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    formatter,
                    "a sequence of {N} length byte arrays Vec<[u8; {N}]>"
                )
            }

            fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
            where
                E: Error,
            {
                if bytes.len() % N != 0 {
                    return Err(E::invalid_length(bytes.len(), &self));
                }
                let mut vec = Vec::with_capacity(bytes.len() / N);
                let ptr = vec.spare_capacity_mut().as_mut_ptr();

                unsafe {
                    ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
                    vec.set_len(bytes.len() / N);
                    ptr::write(self.dst, vec);
                }

                Ok(())
            }
        }

        deserializer.deserialize_bytes(VisitFixedByteVecSeed { dst: self.0 })
    }
}

/// [`DeserializeSeed`] for a byte array of length `N`.
///
/// Allows initializing a fixed length byte array in place.
///
/// # Example
/// ```
/// # use solana_entry::storage::FixedByteSeed;
/// # use serde::{Serialize, Deserialize, Deserializer, ser::SerializeTuple, de::{Visitor, SeqAccess, Error}};
/// # use serde_bytes::Bytes;
/// # use std::{fmt, mem::MaybeUninit};
/// # use rand::prelude::*;
/// # use core::array;
/// #[derive(Debug, PartialEq)]
/// struct Message {
///     hash: [u8; 32],
/// }
///
/// impl Serialize for Message {
///     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
///     where
///         S: serde::Serializer,
///     {
///         let mut seq = serializer.serialize_tuple(1)?;
///         seq.serialize_element(&Bytes::new(&self.hash))?;
///         seq.end()
///     }
/// }
///
/// impl<'de> Deserialize<'de> for Message {
///     fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
///     where
///         D: Deserializer<'de>,
///     {
///         struct VisitMessage;
///
///         impl<'de> Visitor<'de> for VisitMessage {
///             type Value = Message;
///
///             fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
///                 formatter.write_str("a Message")
///             }
///
///             fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
///             where
///                 A: SeqAccess<'de>,
///             {
///                 let mut msg = MaybeUninit::<Message>::uninit();
///                 let msg_ptr = msg.as_mut_ptr();
///                 let hash_ptr = unsafe { &raw mut (*msg_ptr).hash };
///                 // Write the hash directly into the struct
///                 seq.next_element_seed(FixedByteSeed(hash_ptr))?
///                     .ok_or_else(|| A::Error::invalid_length(0, &self))?;
///
///                 Ok(unsafe { msg.assume_init() })
///             }
///         }
///
///         deserializer.deserialize_tuple(1, VisitMessage)
///     }
/// }
///
/// fn main() {
///     let hash = array::from_fn(|_| rand::random());
///     let msg = Message { hash };
///     let serialized = bincode::serialize(&msg).unwrap();
///     let deserialized = bincode::deserialize(&serialized).unwrap();
///     assert_eq!(msg, deserialized);
/// }
/// ```
pub struct FixedByteSeed<const N: usize>(pub *mut [u8; N]);

impl<'de, const N: usize> DeserializeSeed<'de> for FixedByteSeed<N> {
    type Value = ();

    #[inline(always)]
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VisitByteSeed<const N: usize> {
            dst: *mut [u8; N],
        }

        impl<const N: usize> Visitor<'_> for VisitByteSeed<N> {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "a {N} length byte array [u8; {N}]")
            }

            #[inline(always)]
            fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if bytes.len() != N {
                    return Err(E::invalid_length(bytes.len(), &self));
                }
                unsafe {
                    ptr::copy_nonoverlapping(bytes.as_ptr(), self.dst as *mut u8, N);
                }
                Ok(())
            }
        }

        deserializer.deserialize_bytes(VisitByteSeed { dst: self.0 })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::entry::Entry as EntryRef, proptest::prelude::*, solana_address::Address,
        solana_hash::Hash, solana_signature::Signature,
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
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(a, b, c)| MessageHeader {
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

    fn strat_legacy_message() -> impl Strategy<Value = legacy::Message> {
        (
            strat_message_header(),
            proptest::collection::vec(strat_address(), 0..=16),
            strat_hash(),
            proptest::collection::vec(strat_compiled_instruction(), 0..=16),
        )
            .prop_map(|(header, account_keys, recent_blockhash, instructions)| {
                legacy::Message {
                    header,
                    account_keys,
                    recent_blockhash,
                    instructions,
                }
            })
    }

    fn strat_v0_message() -> impl Strategy<Value = v0::Message> {
        (
            strat_message_header(),
            proptest::collection::vec(strat_address(), 0..=16),
            strat_hash(),
            proptest::collection::vec(strat_compiled_instruction(), 0..=16),
            proptest::collection::vec(strat_address_table_lookup(), 0..=8),
        )
            .prop_map(
                |(header, account_keys, recent_blockhash, instructions, address_table_lookups)| {
                    v0::Message {
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

    fn strat_entry_inner() -> impl Strategy<Value = EntryRef> {
        (
            any::<u64>(),
            strat_hash(),
            proptest::collection::vec(strat_versioned_transaction(), 0..=8),
        )
            .prop_map(|(num_hashes, hash, transactions)| EntryRef {
                num_hashes,
                hash,
                transactions,
            })
    }

    fn strat_entry() -> impl Strategy<Value = Entry> {
        strat_entry_inner().prop_map(Entry::from_inner)
    }

    proptest! {
        #[test]
        fn serde_roundtrip(entry in strat_entry()) {
            let serialized = bincode::serialize(&entry).unwrap();
            let deserialized: Entry = bincode::deserialize(&serialized).unwrap();
            prop_assert_eq!(entry, deserialized);
        }

        #[test]
        fn serialization_is_deterministic(entry in strat_entry()) {
            let a = bincode::serialize(&entry).unwrap();
            let b = bincode::serialize(&entry).unwrap();
            prop_assert_eq!(a, b);
        }

        #[test]
        fn ref_does_not_roundtrip(entry in strat_entry_inner()) {
            let bytes = bincode::serialize(&entry).unwrap();
            let res: Result<Entry, _> = bincode::deserialize(&bytes);
            prop_assert!(res.is_err());
        }

        #[test]
        fn outer_does_not_roundtrip(entry in strat_entry()) {
            let bytes = bincode::serialize(&entry).unwrap();
            let res: Result<EntryRef, _> = bincode::deserialize(&bytes);
            prop_assert!(res.is_err());
        }
    }
}
