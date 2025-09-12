use {
    serde::{
        de::{self, DeserializeSeed, Deserializer, Error, SeqAccess, Visitor},
        ser::{SerializeSeq, SerializeTuple, Serializer},
        Deserialize, Serialize,
    },
    serde_bytes::{ByteArray, ByteBuf, Bytes},
    solana_address::{Address, ADDRESS_BYTES},
    solana_hash::{Hash, HASH_BYTES},
    solana_message::{
        legacy,
        v0::{self, MessageAddressTableLookup},
        MessageHeader, VersionedMessage,
    },
    solana_signature::{Signature, SIGNATURE_BYTES},
    solana_transaction::{versioned::VersionedTransaction, CompiledInstruction},
    std::{
        fmt,
        marker::PhantomData,
        mem::{transmute, MaybeUninit},
        ptr::{self},
        result::Result,
    },
};

struct SignaturesSer<'a>(&'a [Signature]);
impl Serialize for SignaturesSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_seq(Some(self.0.len()))?;
        for signature in self.0 {
            s.serialize_element(&Bytes::new(signature.as_ref()))?;
        }
        s.end()
    }
}

struct AddressesSer<'a>(&'a [Address]);
impl Serialize for AddressesSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_seq(Some(self.0.len()))?;
        for address in self.0 {
            s.serialize_element(&Bytes::new(address.as_ref()))?;
        }
        s.end()
    }
}

struct MessageHeaderSer<'a>(&'a MessageHeader);
impl Serialize for MessageHeaderSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let bytes = [
            self.0.num_required_signatures,
            self.0.num_readonly_signed_accounts,
            self.0.num_readonly_unsigned_accounts,
        ];
        serializer.serialize_bytes(&bytes)
    }
}

#[repr(transparent)]
struct MessageHeaderDe(MessageHeader);
impl<'de> Deserialize<'de> for MessageHeaderDe {
    #[inline(always)]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = ByteArray::<3>::deserialize(deserializer)?;
        Ok(MessageHeaderDe(MessageHeader {
            num_required_signatures: bytes[0],
            num_readonly_signed_accounts: bytes[1],
            num_readonly_unsigned_accounts: bytes[2],
        }))
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
                    (&raw mut ((*self.dst).program_id_index)).write(program_id_index);
                    (&raw mut ((*self.dst).accounts)).write(accounts);
                    (&raw mut ((*self.dst).data)).write(data);
                }
                Ok(())
            }
        }

        deserializer.deserialize_seq(VisitCompiledInstruction { dst: self.0 })
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
        s.serialize_element(&MessageHeaderSer(&self.0.header))?;
        s.serialize_element(&AddressesSer(&self.0.account_keys))?;
        s.serialize_element(&Bytes::new(self.0.recent_blockhash.as_bytes()))?;
        s.serialize_element(&CompiledInstructionsSer(&self.0.instructions))?;
        s.end()
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
                let account_key_ptr =
                    unsafe { &raw mut (*self.dst).account_key as *mut [u8; ADDRESS_BYTES] };

                seq.next_element_seed(ByteSeed(account_key_ptr))?
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
                    (&raw mut ((*self.dst).writable_indexes)).write(writeable_indexes);
                    (&raw mut ((*self.dst).readonly_indexes)).write(readonly_indexes);
                }

                Ok(())
            }
        }

        deserializer.deserialize_seq(VisitAddressTableLookup { dst: self.0 })
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
        s.serialize_element(&MessageHeaderSer(&self.0.header))?;
        s.serialize_element(&AddressesSer(&self.0.account_keys))?;
        s.serialize_element(&Bytes::new(self.0.recent_blockhash.as_bytes()))?;
        s.serialize_element(&CompiledInstructionsSer(&self.0.instructions))?;
        s.serialize_element(&AddressTableLookupsSer(&self.0.address_table_lookups))?;
        s.end()
    }
}

#[repr(u32)]
enum VersionedMessageVariant {
    Legacy,
    V0,
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

struct VersionedTransactionSer<'a>(&'a VersionedTransaction);
impl Serialize for VersionedTransactionSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_tuple(2)?;
        s.serialize_element(&SignaturesSer(&self.0.signatures))?;
        s.serialize_element(&VersionedMessageSer(&self.0.message))?;
        s.end()
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

struct VersionedTransactionSeed(*mut VersionedTransaction);

impl<'de> DeserializeSeed<'de> for VersionedTransactionSeed {
    type Value = ();

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
                let sig_ptr =
                    unsafe { &raw mut (*self.dst).signatures } as *mut Vec<[u8; SIGNATURE_BYTES]>;

                seq.next_element_seed(VecSeqSeed::new(sig_ptr, ByteSeed))?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;

                Ok(())
            }
        }

        deserializer.deserialize_seq(VisitVersionedTransaction { dst: self.0 })
    }
}

#[repr(transparent)]
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
                    num_hashes_ptr.write(num_hashes);
                }

                seq.next_element_seed(ByteSeed(hash_ptr))?
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
/// Facilitates initializing a `Vec` in place (i.e., without intermediate copy).
///
/// This works by accepting a "`DeserializeSeed` generator" function
/// that is passed each uninitialized "slot" in the allocated memory.
/// This generator has the signature `Fn(&mut MaybeUninit<T>) -> Seed`, where `Seed`
/// is a [`DeserializeSeed`] for the item type.
struct VecSeqSeed<T, Seed, GenSeed> {
    dst: *mut Vec<T>,
    gen_seed: GenSeed,
    _marker: PhantomData<Seed>,
}

impl<T, Seed, GenSeed> VecSeqSeed<T, Seed, GenSeed> {
    fn new(dst: *mut Vec<T>, gen_seed: GenSeed) -> Self {
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
                formatter.write_str("a Vec")
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
                        *self.dst = Vec::new();
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
                    *self.dst = vec;
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

/// [`DeserializeSeed`] for a byte array of length `N`.
///
/// Allows initializing a fixed length byte array in place (i.e., without intermediate copy).
struct ByteSeed<const N: usize>(*mut [u8; N]);

impl<'de, const N: usize> DeserializeSeed<'de> for ByteSeed<N> {
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
                write!(formatter, "a byte array of length {N}")
            }

            #[inline(always)]
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v.len() != N {
                    return Err(E::invalid_length(v.len(), &self));
                }
                // SAFETY: Length is guaranteed to be `N` and slice from `serde` is non-overlapping with `self.dst`.
                unsafe {
                    ptr::copy_nonoverlapping(v.as_ptr(), self.dst as *mut u8, N);
                }
                Ok(())
            }
        }

        deserializer.deserialize_bytes(VisitByteSeed { dst: self.0 })
    }
}
