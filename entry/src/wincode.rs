//! [`wincode`] type definitions for types that comprise [`crate::entry::Entry`].
//!
//! These definitions should eventually be upstreamed to the solana sdk repository.
use {
    solana_address::Address,
    solana_hash::Hash,
    solana_message::{self, legacy, v0, MESSAGE_VERSION_PREFIX},
    solana_signature::Signature,
    solana_transaction::versioned,
    std::mem::{self, MaybeUninit},
    wincode::{
        containers::{self, Pod},
        error::invalid_tag_encoding,
        io::{Reader, Writer},
        len::ShortU16Len,
        ReadResult, SchemaRead, SchemaWrite, WriteResult,
    },
};

#[derive(SchemaWrite, SchemaRead)]
#[wincode(from = "solana_message::MessageHeader", struct_extensions)]
struct MessageHeader {
    num_required_signatures: u8,
    num_readonly_signed_accounts: u8,
    num_readonly_unsigned_accounts: u8,
}

#[derive(SchemaWrite, SchemaRead)]
#[wincode(from = "solana_transaction::CompiledInstruction")]
struct CompiledInstruction {
    program_id_index: u8,
    accounts: containers::Vec<u8, ShortU16Len>,
    data: containers::Vec<u8, ShortU16Len>,
}

#[derive(SchemaWrite, SchemaRead)]
#[wincode(from = "legacy::Message", struct_extensions)]
struct LegacyMessage {
    header: MessageHeader,
    account_keys: containers::Vec<Pod<Address>, ShortU16Len>,
    recent_blockhash: Pod<Hash>,
    instructions: containers::Vec<CompiledInstruction, ShortU16Len>,
}

#[derive(SchemaWrite, SchemaRead)]
#[wincode(from = "v0::MessageAddressTableLookup")]
struct MessageAddressTableLookup {
    account_key: Pod<Address>,
    writable_indexes: containers::Vec<u8, ShortU16Len>,
    readonly_indexes: containers::Vec<u8, ShortU16Len>,
}

#[derive(SchemaWrite, SchemaRead)]
#[wincode(from = "v0::Message")]
struct V0Message {
    header: MessageHeader,
    account_keys: containers::Vec<Pod<Address>, ShortU16Len>,
    recent_blockhash: Pod<Hash>,
    instructions: containers::Vec<CompiledInstruction, ShortU16Len>,
    address_table_lookups: containers::Vec<MessageAddressTableLookup, ShortU16Len>,
}

#[derive(SchemaWrite, SchemaRead)]
#[wincode(from = "versioned::VersionedTransaction")]
pub(crate) struct VersionedTransaction {
    signatures: containers::Vec<Pod<Signature>, ShortU16Len>,
    message: VersionedMsg,
}

struct VersionedMsg;

impl SchemaWrite for VersionedMsg {
    type Src = solana_message::VersionedMessage;

    #[inline(always)]
    fn size_of(src: &Self::Src) -> WriteResult<usize> {
        match src {
            solana_message::VersionedMessage::Legacy(message) => LegacyMessage::size_of(message),
            // +1 for message version prefix
            solana_message::VersionedMessage::V0(message) => Ok(1 + V0Message::size_of(message)?),
        }
    }

    #[inline(always)]
    fn write(writer: &mut impl Writer, src: &Self::Src) -> WriteResult<()> {
        match src {
            solana_message::VersionedMessage::Legacy(message) => {
                LegacyMessage::write(writer, message)
            }
            solana_message::VersionedMessage::V0(message) => {
                u8::write(writer, &MESSAGE_VERSION_PREFIX)?;
                V0Message::write(writer, message)
            }
        }
    }
}

impl<'de> SchemaRead<'de> for VersionedMsg {
    type Dst = solana_message::VersionedMessage;

    fn read(reader: &mut impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> ReadResult<()> {
        // From `solana_message`:
        //
        // If the first bit is set, the remaining 7 bits will be used to determine
        // which message version is serialized starting from version `0`. If the first
        // is bit is not set, all bytes are used to encode the legacy `Message`
        // format.
        let variant = u8::get(reader)?;

        if variant & MESSAGE_VERSION_PREFIX != 0 {
            let version = variant & !MESSAGE_VERSION_PREFIX;
            return match version {
                0 => {
                    let msg = V0Message::get(reader)?;
                    dst.write(solana_message::VersionedMessage::V0(msg));
                    Ok(())
                }
                _ => Err(invalid_tag_encoding(version as usize)),
            };
        }

        /// A guard that ensures the [`legacy::Message`] is properly dropped on error or panic.
        ///
        /// Fields will be dropped in reverse initialization order.
        ///
        /// This is necessary in particular for [`legacy::Message`] as it contains heap allocated fields.
        /// Namely, `account_keys` and `instructions`, which are `Vec<Address>` and `Vec<CompiledInstruction>`,
        /// respectively. These will leak if not dropped on error or panic.
        struct LegacyMessageDropGuard<'a> {
            inner: &'a mut MaybeUninit<legacy::Message>,
            field_init_count: u8,
        }

        impl<'a> LegacyMessageDropGuard<'a> {
            const fn new(inner: &'a mut MaybeUninit<legacy::Message>) -> Self {
                Self {
                    inner,
                    field_init_count: 0,
                }
            }

            const fn inc_init_count(&mut self) {
                self.field_init_count += 1;
            }
        }

        impl<'a> Drop for LegacyMessageDropGuard<'a> {
            // Fields are initialized in order, matching the serialized format.
            //
            // 0 -> header
            // 1 -> account_keys
            // 2 -> recent_blockhash
            // 3 -> instructions
            //
            // We drop in reverse order to match Rust's drop semantics.
            fn drop(&mut self) {
                use core::ptr;

                // No fields have been initialized.
                if self.field_init_count == 0 {
                    return;
                }

                if self.field_init_count == 4 {
                    // SAFETY: All fields have been initialized, safe to drop the entire message.
                    unsafe { self.inner.assume_init_drop() };
                    return;
                }

                let msg_ptr = self.inner.as_mut_ptr();

                // We don't technically have to worry about recent_blockhash, since it's on the stack,
                // but we do it for completeness.
                if self.field_init_count == 3 {
                    // SAFETY: Recent blockhash is initialized, safe to drop it.
                    unsafe {
                        ptr::drop_in_place(&raw mut (*msg_ptr).recent_blockhash);
                    }
                }
                if self.field_init_count >= 2 {
                    // SAFETY: Account keys are initialized, safe to drop.
                    unsafe {
                        ptr::drop_in_place(&raw mut (*msg_ptr).account_keys);
                    }
                }
                // Similarly to recent_blockhash, we don't technically have to worry about header,
                // but we do it for completeness.
                if self.field_init_count >= 1 {
                    // SAFETY: Header is initialized, safe to drop it.
                    unsafe {
                        ptr::drop_in_place(&raw mut (*msg_ptr).header);
                    }
                }
            }
        }

        let mut msg = MaybeUninit::<legacy::Message>::uninit();
        // We've already read the variant byte which, in the legacy case, represents
        // the `num_required_signatures` field.
        // As such, we need to write the remaining fields into the message manually,
        // as calling `LegacyMessage::read` will miss the first field.
        let header_uninit = LegacyMessage::uninit_header_mut(&mut msg);

        // We don't need to worry about a drop guard for the header,
        // as it's comprised entirely of `u8`s on the stack.
        MessageHeader::write_uninit_num_required_signatures(variant, header_uninit);
        MessageHeader::read_num_readonly_signed_accounts(reader, header_uninit)?;
        MessageHeader::read_num_readonly_unsigned_accounts(reader, header_uninit)?;

        let mut guard = LegacyMessageDropGuard::new(&mut msg);
        // 1. Header is initialized.
        guard.inc_init_count();
        LegacyMessage::read_account_keys(reader, guard.inner)?;
        // 2. Account keys are initialized.
        guard.inc_init_count();
        LegacyMessage::read_recent_blockhash(reader, guard.inner)?;
        // 3. Recent blockhash is initialized.
        guard.inc_init_count();
        LegacyMessage::read_instructions(reader, guard.inner)?;
        // 4. Instructions are initialized.
        guard.inc_init_count();

        // All fields are initialized, safe to drop the the guard.
        mem::forget(guard);

        // SAFETY: All fields are initialized, safe to assume initialized.
        let msg = unsafe { msg.assume_init() };
        dst.write(solana_message::VersionedMessage::Legacy(msg));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::entry::Entry,
        proptest::prelude::*,
        solana_address::{Address, ADDRESS_BYTES},
        solana_hash::{Hash, HASH_BYTES},
        solana_message::{
            legacy::Message as LegacyMessage,
            v0::{self, MessageAddressTableLookup},
            MessageHeader, VersionedMessage,
        },
        solana_signature::{Signature, SIGNATURE_BYTES},
        solana_transaction::{versioned::VersionedTransaction, CompiledInstruction},
        wincode::Deserialize,
    };

    fn strat_byte_vec(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..=max_len)
    }

    fn strat_repeated_byte_vec(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
        (any::<u8>(), 0..=max_len).prop_map(|(b, len)| vec![b; len])
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
        (any::<u8>(), strat_byte_vec(128), strat_byte_vec(128)).prop_map(
            |(program_id_index, accounts, data)| {
                CompiledInstruction::new_from_raw_parts(program_id_index, accounts, data)
            },
        )
    }

    fn strat_address_table_lookup() -> impl Strategy<Value = MessageAddressTableLookup> {
        (strat_address(), strat_byte_vec(128), strat_byte_vec(128)).prop_map(
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
            proptest::collection::vec(strat_address(), 0..=8),
            strat_hash(),
            proptest::collection::vec(strat_compiled_instruction(), 0..=8),
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

    fn strat_v0_message() -> impl Strategy<Value = v0::Message> {
        (
            strat_message_header(),
            proptest::collection::vec(strat_address(), 0..=8),
            strat_hash(),
            proptest::collection::vec(strat_compiled_instruction(), 0..=4),
            proptest::collection::vec(strat_address_table_lookup(), 0..=4),
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
            proptest::collection::vec(strat_signature(), 0..=8),
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
            proptest::collection::vec(strat_versioned_transaction(), 0..=4),
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
        fn deser_fails_on_bad_data(data in strat_repeated_byte_vec(1024)) {
            // represents a zeroed Entry -- valid
            if data.get(0..48) == Some(&[0; 48]) {
                prop_assert!(Entry::deserialize(&data).is_ok());
            } else {
                prop_assert!(Entry::deserialize(&data).is_err());
            }

            // represents a bincode length 0 -- valid
            if data.get(0..8) == Some(&[0; 8]) {
                prop_assert!(<Vec<Entry>>::deserialize(&data).is_ok());
            } else {
                prop_assert!(<Vec<Entry>>::deserialize(&data).is_err());
            }
        }

        #[test]
        fn serialized_size_equivalence(entry in strat_entry()) {
            let serialized = bincode::serialized_size(&entry).unwrap();
            let size = wincode::serialized_size(&entry).unwrap();
            prop_assert_eq!(serialized, size);

        }

        #[test]
        fn serialized_size_multi_equivalence(entries in strat_entries()) {
            let serialized = bincode::serialized_size(&entries).unwrap();
            let size = wincode::serialized_size(&entries).unwrap();
            prop_assert_eq!(serialized, size);
        }

        #[test]
        fn de_equivalence(entry in strat_entry()) {
            let serialized = bincode::serialize(&entry).unwrap();
            let deserialized: Entry = wincode::deserialize(&serialized).unwrap();
            prop_assert_eq!(entry, deserialized);
        }

        #[test]
        fn de_multi_equivalence(entries in strat_entries()) {
            let serialized = bincode::serialize(&entries).unwrap();
            let deserialized: Vec<Entry> = wincode::deserialize(&serialized).unwrap();
            prop_assert_eq!(entries, deserialized);
        }

        #[test]
        fn ser_equivalence(entry in strat_entry()) {
            let serialized = wincode::serialize(&entry).unwrap();
            prop_assert_eq!(serialized, bincode::serialize(&entry).unwrap());
        }

        #[test]
        fn ser_multi_equivalence(entries in strat_entries()) {
            let serialized = wincode::serialize(&entries).unwrap();
            prop_assert_eq!(serialized, bincode::serialize(&entries).unwrap());
        }

        #[test]
        fn roundtrip(entry in strat_entry()) {
            let serialized = wincode::serialize(&entry).unwrap();
            let deserialized: Entry = wincode::deserialize(&serialized).unwrap();
            prop_assert_eq!(&entry, &deserialized);
        }

        #[test]
        fn roundtrip_multi(entries in strat_entries()) {
            let serialized = wincode::serialize(&entries).unwrap();
            let deserialized: Vec<Entry> = wincode::deserialize(&serialized).unwrap();
            prop_assert_eq!(entries, deserialized);
        }
    }
}
