use {
    crate::{
        address_table_lookup_frame::{AddressTableLookupFrame, AddressTableLookupIterator},
        bytes::{
            advance_offset_for_array, advance_offset_for_type, check_remaining,
            unchecked_copy_value, unchecked_read_byte,
        },
        instructions_frame::{InstructionsFrame, InstructionsIterator},
        message_header_frame::MessageHeaderFrame,
        result::{Result, TransactionViewError},
        signature_frame::SignatureFrame,
        static_account_keys_frame::StaticAccountKeysFrame,
        transaction_config_frame::TransactionConfigFrame,
        transaction_version::TransactionVersion,
    },
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
};

#[derive(Debug)]
pub(crate) struct TransactionFrame {
    /// Signature framing data.
    signature: SignatureFrame,
    /// Message header framing data.
    message_header: MessageHeaderFrame,
    /// Static account keys framing data.
    static_account_keys: StaticAccountKeysFrame,
    /// Recent blockhash offset.
    recent_blockhash_offset: u16,
    /// Instructions framing data.
    instructions: InstructionsFrame,
    /// Address table lookup framing data.
    address_table_lookup: AddressTableLookupFrame,
    /// Transaction config framing data
    transaction_config_frame: TransactionConfigFrame,
    /// The data length in bytes
    data_len: u16,
}

impl TransactionFrame {
    /// Parse a serialized transaction and verify basic structure.
    /// The `bytes` parameter must have no trailing data.
    pub(crate) fn try_new(bytes: &[u8]) -> Result<Self> {
        if Self::is_legacy_or_v0(bytes)? {
            Self::try_new_as_legacy_or_v0(bytes)
        } else {
            Self::try_new_as_v1(bytes)
        }
    }

    fn try_new_as_legacy_or_v0(bytes: &[u8]) -> Result<Self> {
        let mut offset = 0;
        let signature = SignatureFrame::try_new(bytes, &mut offset)?;
        let message_header = MessageHeaderFrame::try_new(bytes, &mut offset)?;
        let static_account_keys = StaticAccountKeysFrame::try_new(bytes, &mut offset)?;

        // The recent blockhash is the first account key after the static
        // account keys. The recent blockhash is always present in a valid
        // transaction and has a fixed size of 32 bytes.
        let recent_blockhash_offset = offset as u16;
        advance_offset_for_type::<Hash>(bytes, &mut offset)?;

        let instructions = InstructionsFrame::try_new_for_legacy_and_v0(bytes, &mut offset)?;
        let address_table_lookup = match message_header.version {
            TransactionVersion::Legacy => AddressTableLookupFrame {
                num_address_table_lookups: 0,
                offset: 0,
                total_writable_lookup_accounts: 0,
                total_readonly_lookup_accounts: 0,
            },
            TransactionVersion::V0 => AddressTableLookupFrame::try_new(bytes, &mut offset)?,
            TransactionVersion::V1 => unreachable!("unexpected variant"),
        };

        // Verify that the entire transaction was parsed.
        if offset != bytes.len() {
            return Err(TransactionViewError::ParseError);
        }

        Ok(Self {
            signature,
            message_header,
            static_account_keys,
            recent_blockhash_offset,
            instructions,
            address_table_lookup,
            transaction_config_frame: TransactionConfigFrame::not_applicable(),
            data_len: offset as u16,
        })
    }

    fn try_new_as_v1(bytes: &[u8]) -> Result<Self> {
        let mut offset: usize = 0;

        // Fixed-size txv1 prefix up through NumAddresses:
        // VersionByte (u8)
        // LegacyHeader (u8, u8, u8)
        // TransactionConfigMask (u32)
        // LifetimeSpecifier ([u8; 32])
        // NumInstructions (u8)
        // NumAddresses (u8)
        const FIXED_V1_PREFIX_LEN: usize = 1 + 3 + 4 + core::mem::size_of::<Hash>() + 1 + 1;

        check_remaining(bytes, offset, FIXED_V1_PREFIX_LEN)?;

        // SAFETY: have checked bytes have enough space for preifx all the way up to
        //         NumAddresses.

        // message offset would be the first byte of txv1 packet, which is version byte
        let message_offset = offset as u16;
        // Version Byte
        let version = unsafe { unchecked_read_byte(bytes, &mut offset) };
        let version = match version & !solana_message::MESSAGE_VERSION_PREFIX {
            1 => TransactionVersion::V1,
            _ => return Err(TransactionViewError::ParseError),
        };
        // Legacy Header
        let num_required_signatures = unsafe { unchecked_read_byte(bytes, &mut offset) };
        let num_readonly_signed_accounts = unsafe { unchecked_read_byte(bytes, &mut offset) };
        let num_readonly_unsigned_accounts = unsafe { unchecked_read_byte(bytes, &mut offset) };
        // Transaction Config Bit Mask
        let transaction_config_mask_offset = offset;
        let transaction_config_mask: u32 = unsafe { unchecked_copy_value(bytes, offset) };
        offset = offset.wrapping_add(core::mem::size_of::<u32>());
        // Lifetime specifier
        let recent_blockhash_offset = offset as u16;
        offset = offset.wrapping_add(core::mem::size_of::<Hash>());
        // Num instructions and addresses
        let num_instructions = unsafe { unchecked_read_byte(bytes, &mut offset) };
        let num_addresses = unsafe { unchecked_read_byte(bytes, &mut offset) };

        // addresses
        let addresses_offset = offset as u16;
        advance_offset_for_array::<Pubkey>(bytes, &mut offset, u16::from(num_addresses))?;
        // config value slots: one 4-byte slot per set bit in mask
        let transaction_config_frame = TransactionConfigFrame::try_new(
            bytes,
            transaction_config_mask_offset,
            transaction_config_mask,
            &mut offset,
        )?;
        // instruction headers and payloads
        let instructions = InstructionsFrame::try_new_for_v1(bytes, &mut offset, num_instructions)?;
        // signatures
        let signatures_offset = offset as u16;
        advance_offset_for_array::<Signature>(
            bytes,
            &mut offset,
            u16::from(num_required_signatures),
        )?;
        // Verify that the entire transaction was parsed.
        if offset != bytes.len() {
            return Err(TransactionViewError::ParseError);
        }

        let frame = Self {
            signature: SignatureFrame {
                num_signatures: num_required_signatures,
                offset: signatures_offset,
            },
            message_header: MessageHeaderFrame {
                offset: message_offset,
                version,
                num_required_signatures,
                num_readonly_signed_accounts,
                num_readonly_unsigned_accounts,
            },
            static_account_keys: StaticAccountKeysFrame {
                num_static_accounts: num_addresses, // always static accounts in txv1
                offset: addresses_offset,
            },
            recent_blockhash_offset,
            instructions,
            // Don't have ATL in txv1
            address_table_lookup: AddressTableLookupFrame {
                num_address_table_lookups: 0,
                offset: 0,
                total_writable_lookup_accounts: 0,
                total_readonly_lookup_accounts: 0,
            },
            transaction_config_frame,
            data_len: offset as u16,
        };

        Ok(frame)
    }

    fn is_legacy_or_v0(bytes: &[u8]) -> Result<bool> {
        let first_byte = *bytes.first().ok_or(TransactionViewError::ParseError)?;

        // In wire format:
        // - Legacy/v0 transactions start with signatures (compact-u16 count).
        //   Packet size limits keep the signature count well below 128, so the
        //   first byte never has MSB set.
        // - v1 transactions start with a version byte with MSB = 1.
        Ok((first_byte & solana_message::MESSAGE_VERSION_PREFIX) == 0)
    }

    /// Return the number of signatures in the transaction.
    #[inline]
    pub(crate) fn num_signatures(&self) -> u8 {
        self.signature.num_signatures
    }

    /// Return the version of the transaction.
    #[inline]
    pub(crate) fn version(&self) -> TransactionVersion {
        self.message_header.version
    }

    /// Return the number of required signatures in the transaction.
    #[inline]
    pub(crate) fn num_required_signatures(&self) -> u8 {
        self.message_header.num_required_signatures
    }

    /// Return the number of readonly signed static accounts in the transaction.
    #[inline]
    pub(crate) fn num_readonly_signed_static_accounts(&self) -> u8 {
        self.message_header.num_readonly_signed_accounts
    }

    /// Return the number of readonly unsigned static accounts in the transaction.
    #[inline]
    pub(crate) fn num_readonly_unsigned_static_accounts(&self) -> u8 {
        self.message_header.num_readonly_unsigned_accounts
    }

    /// Return the number of static account keys in the transaction.
    #[inline]
    pub(crate) fn num_static_account_keys(&self) -> u8 {
        self.static_account_keys.num_static_accounts
    }

    /// Return the number of instructions in the transaction.
    #[inline]
    pub(crate) fn num_instructions(&self) -> u16 {
        self.instructions.num_instructions()
    }

    /// Return the number of address table lookups in the transaction.
    #[inline]
    pub(crate) fn num_address_table_lookups(&self) -> u8 {
        self.address_table_lookup.num_address_table_lookups
    }

    /// Return the number of writable lookup accounts in the transaction.
    #[inline]
    pub(crate) fn total_writable_lookup_accounts(&self) -> u16 {
        self.address_table_lookup.total_writable_lookup_accounts
    }

    /// Return the number of readonly lookup accounts in the transaction.
    #[inline]
    pub(crate) fn total_readonly_lookup_accounts(&self) -> u16 {
        self.address_table_lookup.total_readonly_lookup_accounts
    }

    /// Return the range to the message as [begin, end]
    #[inline]
    pub(crate) fn message_range(&self) -> (u16, u16) {
        let end = match self.version() {
            TransactionVersion::V1 => self.signature.offset,
            _ => self.data_len,
        };
        (self.message_header.offset, end)
    }

    /// Return transaction_config_frame
    #[inline]
    pub(crate) fn transaction_config_frame(&self) -> &TransactionConfigFrame {
        &self.transaction_config_frame
    }
}

// Separate implementation for `unsafe` accessor methods.
impl TransactionFrame {
    /// Return the slice of signatures in the transaction.
    /// # Safety
    ///   - This function must be called with the same `bytes` slice that was
    ///     used to create the `TransactionFrame` instance.
    #[inline]
    pub(crate) unsafe fn signatures<'a>(&self, bytes: &'a [u8]) -> &'a [Signature] {
        // Verify at compile time there are no alignment constraints.
        const _: () = assert!(
            core::mem::align_of::<Signature>() == 1,
            "Signature alignment"
        );
        // The length of the slice is not greater than isize::MAX.
        const _: () =
            assert!(u8::MAX as usize * core::mem::size_of::<Signature>() <= isize::MAX as usize);

        // SAFETY:
        // - If this `TransactionFrame` was created from `bytes`:
        //     - the pointer is valid for the range and is properly aligned.
        // - `num_signatures` has been verified against the bounds if
        //   `TransactionFrame` was created successfully.
        // - `Signature` are just byte arrays; there is no possibility the
        //   `Signature` are not initialized properly.
        // - The lifetime of the returned slice is the same as the input
        //   `bytes`. This means it will not be mutated or deallocated while
        //   holding the slice.
        // - The length does not overflow `isize`.
        unsafe {
            core::slice::from_raw_parts(
                bytes.as_ptr().add(usize::from(self.signature.offset)) as *const Signature,
                usize::from(self.signature.num_signatures),
            )
        }
    }

    /// Return the slice of static account keys in the transaction.
    ///
    /// # Safety
    ///  - This function must be called with the same `bytes` slice that was
    ///    used to create the `TransactionFrame` instance.
    #[inline]
    pub(crate) unsafe fn static_account_keys<'a>(&self, bytes: &'a [u8]) -> &'a [Pubkey] {
        // Verify at compile time there are no alignment constraints.
        const _: () = assert!(core::mem::align_of::<Pubkey>() == 1, "Pubkey alignment");
        // The length of the slice is not greater than isize::MAX.
        const _: () =
            assert!(u8::MAX as usize * core::mem::size_of::<Pubkey>() <= isize::MAX as usize);

        // SAFETY:
        // - If this `TransactionFrame` was created from `bytes`:
        //     - the pointer is valid for the range and is properly aligned.
        // - `num_static_accounts` has been verified against the bounds if
        //   `TransactionFrame` was created successfully.
        // - `Pubkey` are just byte arrays; there is no possibility the
        //   `Pubkey` are not initialized properly.
        // - The lifetime of the returned slice is the same as the input
        //   `bytes`. This means it will not be mutated or deallocated while
        //   holding the slice.
        // - The length does not overflow `isize`.
        unsafe {
            core::slice::from_raw_parts(
                bytes
                    .as_ptr()
                    .add(usize::from(self.static_account_keys.offset))
                    as *const Pubkey,
                usize::from(self.static_account_keys.num_static_accounts),
            )
        }
    }

    /// Return the recent blockhash in the transaction.
    /// # Safety
    /// - This function must be called with the same `bytes` slice that was
    ///   used to create the `TransactionFrame` instance.
    #[inline]
    pub(crate) unsafe fn recent_blockhash<'a>(&self, bytes: &'a [u8]) -> &'a Hash {
        // Verify at compile time there are no alignment constraints.
        const _: () = assert!(core::mem::align_of::<Hash>() == 1, "Hash alignment");

        // SAFETY:
        // - The pointer is correctly aligned (no alignment constraints).
        // - `Hash` is just a byte array; there is no possibility the `Hash`
        //   is not initialized properly.
        // - Aliasing rules are respected because the lifetime of the returned
        //   reference is the same as the input/source `bytes`.
        unsafe {
            &*(bytes
                .as_ptr()
                .add(usize::from(self.recent_blockhash_offset)) as *const Hash)
        }
    }

    /// Return an iterator over the instructions in the transaction.
    /// # Safety
    /// - This function must be called with the same `bytes` slice that was
    ///   used to create the `TransactionFrame` instance.
    #[inline]
    pub(crate) unsafe fn instructions_iter<'a>(
        &'a self,
        bytes: &'a [u8],
    ) -> InstructionsIterator<'a> {
        self.instructions.iter(bytes)
    }

    /// Return an iterator over the address table lookups in the transaction.
    /// # Safety
    /// - This function must be called with the same `bytes` slice that was
    ///   used to create the `TransactionFrame` instance.
    #[inline]
    pub(crate) unsafe fn address_table_lookup_iter<'a>(
        &self,
        bytes: &'a [u8],
    ) -> AddressTableLookupIterator<'a> {
        AddressTableLookupIterator {
            bytes,
            offset: usize::from(self.address_table_lookup.offset),
            num_address_table_lookups: self.address_table_lookup.num_address_table_lookups,
            index: 0,
        }
    }
}

#[cfg(test)]
impl TransactionFrame {
    pub(crate) fn message_offset(&self) -> u16 {
        self.message_header.offset
    }

    pub(crate) fn signatures_offset(&self) -> u16 {
        self.signature.offset
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_message::{
            AddressLookupTableAccount, Message, MessageHeader, VersionedMessage,
            compiled_instruction::CompiledInstruction, v0, v1,
        },
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_system_interface::instruction::{self as system_instruction, SystemInstruction},
        solana_transaction::versioned::VersionedTransaction,
    };

    fn verify_transaction_view_frame(tx: &VersionedTransaction) {
        let bytes = wincode::serialize(tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        assert_eq!(frame.signature.num_signatures, tx.signatures.len() as u8);
        assert_eq!(frame.signature.offset as usize, 1);

        assert_eq!(
            frame.message_header.num_required_signatures,
            tx.message.header().num_required_signatures
        );
        assert_eq!(
            frame.message_header.num_readonly_signed_accounts,
            tx.message.header().num_readonly_signed_accounts
        );
        assert_eq!(
            frame.message_header.num_readonly_unsigned_accounts,
            tx.message.header().num_readonly_unsigned_accounts
        );

        assert_eq!(
            frame.static_account_keys.num_static_accounts,
            tx.message.static_account_keys().len() as u8
        );
        assert_eq!(
            frame.instructions.num_instructions(),
            tx.message.instructions().len() as u16
        );
        assert_eq!(
            frame.address_table_lookup.num_address_table_lookups,
            tx.message
                .address_table_lookups()
                .map(|x| x.len() as u8)
                .unwrap_or(0)
        );
    }

    fn minimally_sized_transaction() -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::Legacy(Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                account_keys: vec![Pubkey::default()],
                recent_blockhash: Hash::default(),
                instructions: vec![],
            }),
        }
    }

    fn simple_transfer() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::Legacy(Message::new(
                &[system_instruction::transfer(
                    &payer,
                    &Pubkey::new_unique(),
                    1,
                )],
                Some(&payer),
            )),
        }
    }

    fn simple_transfer_v0() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::V0(
                v0::Message::try_compile(
                    &payer,
                    &[system_instruction::transfer(
                        &payer,
                        &Pubkey::new_unique(),
                        1,
                    )],
                    &[],
                    Hash::default(),
                )
                .unwrap(),
            ),
        }
    }

    fn multiple_transfers() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::Legacy(Message::new(
                &[
                    system_instruction::transfer(&payer, &Pubkey::new_unique(), 1),
                    system_instruction::transfer(&payer, &Pubkey::new_unique(), 1),
                ],
                Some(&payer),
            )),
        }
    }

    fn v0_with_single_lookup() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::V0(
                v0::Message::try_compile(
                    &payer,
                    &[system_instruction::transfer(&payer, &to, 1)],
                    &[AddressLookupTableAccount {
                        key: Pubkey::new_unique(),
                        addresses: vec![to],
                    }],
                    Hash::default(),
                )
                .unwrap(),
            ),
        }
    }

    fn v0_with_multiple_lookups() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        let to1 = Pubkey::new_unique();
        let to2 = Pubkey::new_unique();
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::V0(
                v0::Message::try_compile(
                    &payer,
                    &[
                        system_instruction::transfer(&payer, &to1, 1),
                        system_instruction::transfer(&payer, &to2, 1),
                    ],
                    &[
                        AddressLookupTableAccount {
                            key: Pubkey::new_unique(),
                            addresses: vec![to1],
                        },
                        AddressLookupTableAccount {
                            key: Pubkey::new_unique(),
                            addresses: vec![to2],
                        },
                    ],
                    Hash::default(),
                )
                .unwrap(),
            ),
        }
    }

    fn simple_v1_transaction() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        let program = Pubkey::new_unique();
        let other = Pubkey::new_unique();

        VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V1(v1::Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                config: v1::TransactionConfig {
                    priority_fee: Some(123),
                    compute_unit_limit: Some(456),
                    loaded_accounts_data_size_limit: Some(789),
                    heap_size: Some(1024),
                },
                lifetime_specifier: Hash::default(),
                account_keys: vec![payer, other, program],
                instructions: vec![
                    CompiledInstruction {
                        program_id_index: 2,
                        accounts: vec![0, 1],
                        data: vec![10, 11, 12],
                    },
                    CompiledInstruction {
                        program_id_index: 2,
                        accounts: vec![],
                        data: vec![99],
                    },
                ],
            }),
        }
    }

    #[test]
    fn test_minimal_sized_transaction() {
        verify_transaction_view_frame(&minimally_sized_transaction());
    }

    #[test]
    fn test_simple_transfer() {
        verify_transaction_view_frame(&simple_transfer());
    }

    #[test]
    fn test_simple_transfer_v0() {
        verify_transaction_view_frame(&simple_transfer_v0());
    }

    #[test]
    fn test_v0_with_lookup() {
        verify_transaction_view_frame(&v0_with_single_lookup());
    }

    #[test]
    fn test_trailing_byte() {
        let tx = simple_transfer();
        let mut bytes = wincode::serialize(&tx).unwrap();
        bytes.push(0);
        assert!(TransactionFrame::try_new(&bytes).is_err());
    }

    #[test]
    fn test_insufficient_bytes() {
        let tx = simple_transfer();
        let bytes = wincode::serialize(&tx).unwrap();
        assert!(TransactionFrame::try_new(&bytes[..bytes.len().wrapping_sub(1)]).is_err());
    }

    #[test]
    fn test_signature_overflow() {
        let tx = simple_transfer();
        let mut bytes = wincode::serialize(&tx).unwrap();
        // Set the number of signatures to u16::MAX
        bytes[0] = 0xff;
        bytes[1] = 0xff;
        bytes[2] = 0xff;
        assert!(TransactionFrame::try_new(&bytes).is_err());
    }

    #[test]
    fn test_account_key_overflow() {
        let tx = simple_transfer();
        let mut bytes = wincode::serialize(&tx).unwrap();
        // Set the number of accounts to u16::MAX
        let offset = 1 + core::mem::size_of::<Signature>() + 3;
        bytes[offset] = 0xff;
        bytes[offset + 1] = 0xff;
        bytes[offset + 2] = 0xff;
        assert!(TransactionFrame::try_new(&bytes).is_err());
    }

    #[test]
    fn test_instructions_overflow() {
        let tx = simple_transfer();
        let mut bytes = wincode::serialize(&tx).unwrap();
        // Set the number of instructions to u16::MAX
        let offset = 1
            + core::mem::size_of::<Signature>()
            + 3
            + 1
            + 3 * core::mem::size_of::<Pubkey>()
            + core::mem::size_of::<Hash>();
        bytes[offset] = 0xff;
        bytes[offset + 1] = 0xff;
        bytes[offset + 2] = 0xff;
        assert!(TransactionFrame::try_new(&bytes).is_err());
    }

    #[test]
    fn test_alt_overflow() {
        let tx = simple_transfer_v0();
        let ix_bytes = tx.message.instructions()[0].data.len();
        let mut bytes = wincode::serialize(&tx).unwrap();
        // Set the number of instructions to u16::MAX
        let offset = 1 // byte for num signatures
            + core::mem::size_of::<Signature>() // signature
            + 1 // version byte
            + 3 // message header
            + 1 // byte for num account keys
            + 3 * core::mem::size_of::<Pubkey>() // account keys
            + core::mem::size_of::<Hash>() // recent blockhash
            + 1 // byte for num instructions
            + 1 // program index
            + 1 // byte for num accounts
            + 2 // bytes for account index
            + 1 // byte for data length
            + ix_bytes;
        bytes[offset] = 0x01;
        assert!(TransactionFrame::try_new(&bytes).is_err());
    }

    #[test]
    fn test_basic_accessors() {
        let tx = simple_transfer();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        assert_eq!(frame.num_signatures(), 1);
        assert!(matches!(frame.version(), TransactionVersion::Legacy));
        assert_eq!(frame.num_required_signatures(), 1);
        assert_eq!(frame.num_readonly_signed_static_accounts(), 0);
        assert_eq!(frame.num_readonly_unsigned_static_accounts(), 1);
        assert_eq!(frame.num_static_account_keys(), 3);
        assert_eq!(frame.num_instructions(), 1);
        assert_eq!(frame.num_address_table_lookups(), 0);

        // SAFETY: `bytes` is the same slice used to create `frame`.
        unsafe {
            let signatures = frame.signatures(&bytes);
            assert_eq!(signatures, &tx.signatures);

            let static_account_keys = frame.static_account_keys(&bytes);
            assert_eq!(static_account_keys, tx.message.static_account_keys());

            let recent_blockhash = frame.recent_blockhash(&bytes);
            assert_eq!(recent_blockhash, tx.message.recent_blockhash());
        }
    }

    #[test]
    fn test_instructions_iter_empty() {
        let tx = minimally_sized_transaction();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        // SAFETY: `bytes` is the same slice used to create `frame`.
        unsafe {
            let mut iter = frame.instructions_iter(&bytes);
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_instructions_iter_single() {
        let tx = simple_transfer();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        // SAFETY: `bytes` is the same slice used to create `frame`.
        unsafe {
            let mut iter = frame.instructions_iter(&bytes);
            let ix = iter.next().unwrap();
            assert_eq!(ix.program_id_index, 2);
            assert_eq!(ix.accounts, &[0, 1]);
            assert_eq!(
                ix.data,
                &wincode::serialize(&SystemInstruction::Transfer { lamports: 1 }).unwrap()
            );
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_instructions_iter_multiple() {
        let tx = multiple_transfers();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        // SAFETY: `bytes` is the same slice used to create `frame`.
        unsafe {
            let mut iter = frame.instructions_iter(&bytes);
            let ix = iter.next().unwrap();
            assert_eq!(ix.program_id_index, 3);
            assert_eq!(ix.accounts, &[0, 1]);
            assert_eq!(
                ix.data,
                &wincode::serialize(&SystemInstruction::Transfer { lamports: 1 }).unwrap()
            );
            let ix = iter.next().unwrap();
            assert_eq!(ix.program_id_index, 3);
            assert_eq!(ix.accounts, &[0, 2]);
            assert_eq!(
                ix.data,
                &wincode::serialize(&SystemInstruction::Transfer { lamports: 1 }).unwrap()
            );
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_address_table_lookup_iter_empty() {
        let tx = simple_transfer();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        // SAFETY: `bytes` is the same slice used to create `frame`.
        unsafe {
            let mut iter = frame.address_table_lookup_iter(&bytes);
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_address_table_lookup_iter_single() {
        let tx = v0_with_single_lookup();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        let atls_actual = tx.message.address_table_lookups().unwrap();
        // SAFETY: `bytes` is the same slice used to create `frame`.
        unsafe {
            let mut iter = frame.address_table_lookup_iter(&bytes);
            let lookup = iter.next().unwrap();
            assert_eq!(lookup.account_key, &atls_actual[0].account_key);
            assert_eq!(lookup.writable_indexes, atls_actual[0].writable_indexes);
            assert_eq!(lookup.readonly_indexes, atls_actual[0].readonly_indexes);
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_address_table_lookup_iter_multiple() {
        let tx = v0_with_multiple_lookups();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        let atls_actual = tx.message.address_table_lookups().unwrap();
        // SAFETY: `bytes` is the same slice used to create `frame`.
        unsafe {
            let mut iter = frame.address_table_lookup_iter(&bytes);

            let lookup = iter.next().unwrap();
            assert_eq!(lookup.account_key, &atls_actual[0].account_key);
            assert_eq!(lookup.writable_indexes, atls_actual[0].writable_indexes);
            assert_eq!(lookup.readonly_indexes, atls_actual[0].readonly_indexes);

            let lookup = iter.next().unwrap();
            assert_eq!(lookup.account_key, &atls_actual[1].account_key);
            assert_eq!(lookup.writable_indexes, atls_actual[1].writable_indexes);
            assert_eq!(lookup.readonly_indexes, atls_actual[1].readonly_indexes);

            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_v1_transaction_frame_parses() {
        let tx = simple_v1_transaction();
        let bytes = wincode::serialize(&tx).unwrap();

        let frame = TransactionFrame::try_new(&bytes).unwrap();

        assert!(matches!(frame.version(), TransactionVersion::V1));
        assert_eq!(frame.num_signatures(), 1);
        assert_eq!(frame.num_required_signatures(), 1);
        assert_eq!(frame.num_readonly_signed_static_accounts(), 0);
        assert_eq!(frame.num_readonly_unsigned_static_accounts(), 1);
        assert_eq!(frame.num_static_account_keys(), 3);
        assert_eq!(frame.num_instructions(), 2);

        // txv1 should not have ALTs
        assert_eq!(frame.num_address_table_lookups(), 0);
        assert_eq!(frame.total_writable_lookup_accounts(), 0);
        assert_eq!(frame.total_readonly_lookup_accounts(), 0);

        // new v1-only frame metadata
        assert!(frame.signatures_offset() > frame.message_offset());
    }

    #[test]
    fn test_v1_is_not_legacy_or_v0() {
        let tx = simple_v1_transaction();
        let bytes = wincode::serialize(&tx).unwrap();

        assert!(!TransactionFrame::is_legacy_or_v0(&bytes).unwrap());
    }

    #[test]
    fn test_legacy_is_legacy_or_v0() {
        let payer = Pubkey::new_unique();
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(solana_message::Message::new(&[], Some(&payer))),
        };
        let bytes = wincode::serialize(&tx).unwrap();

        assert!(TransactionFrame::is_legacy_or_v0(&bytes).unwrap());
    }

    #[test]
    fn test_is_legacy_or_v0_empty_bytes() {
        assert!(matches!(
            TransactionFrame::is_legacy_or_v0(&[]),
            Err(TransactionViewError::ParseError),
        ));
    }

    #[test]
    fn test_v1_rejects_unknown_version() {
        let tx = simple_v1_transaction();
        let mut bytes = wincode::serialize(&tx).unwrap();

        // First byte is version-tagged for versioned messages.
        // Flip underlying version to an unsupported value.
        bytes[0] = solana_message::MESSAGE_VERSION_PREFIX | 2;

        assert!(matches!(
            TransactionFrame::try_new(&bytes),
            Err(TransactionViewError::ParseError),
        ));
    }

    #[test]
    fn test_v1_rejects_trailing_byte() {
        let tx = simple_v1_transaction();
        let mut bytes = wincode::serialize(&tx).unwrap();
        bytes.push(0);

        assert!(matches!(
            TransactionFrame::try_new(&bytes),
            Err(TransactionViewError::ParseError),
        ));
    }

    #[test]
    fn test_v1_rejects_truncated_bytes() {
        let tx = simple_v1_transaction();
        let bytes = wincode::serialize(&tx).unwrap();

        assert!(matches!(
            TransactionFrame::try_new(&bytes[..bytes.len() - 1]),
            Err(TransactionViewError::ParseError),
        ));
    }

    #[test]
    fn test_v1_instruction_iteration() {
        let tx = simple_v1_transaction();
        let bytes = wincode::serialize(&tx).unwrap();
        let frame = TransactionFrame::try_new(&bytes).unwrap();

        let mut iter = unsafe { frame.instructions_iter(&bytes) };

        let ix0 = iter.next().unwrap();
        assert_eq!(ix0.program_id_index, 2);
        assert_eq!(ix0.accounts, &[0, 1]);
        assert_eq!(ix0.data, &[10, 11, 12]);

        let ix1 = iter.next().unwrap();
        assert_eq!(ix1.program_id_index, 2);
        assert_eq!(ix1.accounts, &[] as &[u8]);
        assert_eq!(ix1.data, &[99]);

        assert!(iter.next().is_none());
    }
}
