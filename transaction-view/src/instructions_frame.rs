use {
    crate::{
        bytes::{
            advance_offset_for_array, check_remaining, optimized_read_compressed_u16, read_byte,
            unchecked_copy_value, unchecked_read_byte, unchecked_read_slice_data,
        },
        result::{Result, TransactionViewError},
    },
    core::fmt::{Debug, Formatter},
    solana_svm_transaction::instruction::SVMInstruction,
};

/// Contains metadata about the instructions in a transaction packet.
#[derive(Debug)]
pub(crate) enum InstructionsFrame {
    LegacyAndV0 {
        /// The number of instructions in the transaction.
        num_instructions: u16,
        /// The offset to the first instruction in the transaction.
        offset: u16,
        frames: Vec<LegacyAndV0InstructionFrame>,
    },
    V1 {
        num_instructions: u16,
        headers_offset: u16,
        payloads_offset: u16,
    },
}

#[derive(Debug)]
pub struct LegacyAndV0InstructionFrame {
    num_accounts: u16,
    data_len: u16,
    num_accounts_len: u8, // either 1 or 2
    data_len_len: u8,     // either 1 or 2
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Debug)]
struct V1InstructionHeader {
    program_id_index: u8,
    num_accounts: u8,
    data_len: u16,
}

impl InstructionsFrame {
    /// Get the number of instructions and offset to the first instruction.
    /// The offset will be updated to point to the first byte after the last
    /// instruction.
    /// This function will parse each individual instruction to ensure the
    /// instruction data is well-formed, but will not cache data related to
    /// these instructions.
    #[inline(always)]
    pub(crate) fn try_new_for_legacy_and_v0(bytes: &[u8], offset: &mut usize) -> Result<Self> {
        // Read the number of instructions at the current offset.
        // Each instruction needs at least 3 bytes, so do a sanity check here to
        // ensure we have enough bytes to read the number of instructions.
        let num_instructions = optimized_read_compressed_u16(bytes, offset)?;
        check_remaining(
            bytes,
            *offset,
            3usize.wrapping_mul(usize::from(num_instructions)),
        )?;

        // We know the offset does not exceed packet length, and our packet
        // length is less than u16::MAX, so we can safely cast to u16.
        let instructions_offset = *offset as u16;

        // Pre-allocate buffer for frames.
        let mut frames = Vec::with_capacity(usize::from(num_instructions));

        // The instructions do not have a fixed size. So we must iterate over
        // each instruction to find the total size of the instructions,
        // and check for any malformed instructions or buffer overflows.
        for _index in 0..num_instructions {
            // Each instruction has 3 pieces:
            // 1. Program ID index (u8)
            // 2. Accounts indexes ([u8])
            // 3. Data ([u8])

            // Read the program ID index.
            let _program_id_index = read_byte(bytes, offset)?;

            // Read the number of account indexes, and then update the offset
            // to skip over the account indexes.
            let num_accounts_offset = *offset;
            let num_accounts = optimized_read_compressed_u16(bytes, offset)?;
            let num_accounts_len = offset.wrapping_sub(num_accounts_offset) as u8;
            advance_offset_for_array::<u8>(bytes, offset, num_accounts)?;

            // Read the length of the data, and then update the offset to skip
            // over the data.
            let data_len_offset = *offset;
            let data_len = optimized_read_compressed_u16(bytes, offset)?;
            let data_len_len = offset.wrapping_sub(data_len_offset) as u8;
            advance_offset_for_array::<u8>(bytes, offset, data_len)?;

            frames.push(LegacyAndV0InstructionFrame {
                num_accounts,
                num_accounts_len,
                data_len,
                data_len_len,
            });
        }

        Ok(Self::LegacyAndV0 {
            num_instructions,
            offset: instructions_offset,
            frames,
        })
    }

    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn try_new_for_v1(
        bytes: &[u8],
        offset: &mut usize,
        num_instructions: u8,
    ) -> Result<Self> {
        let headers_offset = *offset as u16;
        let headers_len =
            core::mem::size_of::<V1InstructionHeader>().wrapping_mul(num_instructions as usize);

        check_remaining(bytes, *offset, headers_len)?;

        let mut header_offset = *offset;
        *offset = offset.wrapping_add(headers_len);

        let payloads_offset = *offset as u16;

        // Tx v1 stores all instruction payloads contiguously after the header block.
        // We validate headers first, accumulate the total payload size across all
        // instructions, and then do a single bounds check for the whole payload region
        // instead of one bounds check per instruction.
        let mut total_payload_len: usize = 0;
        for _ in 0..num_instructions {
            // SAFETY: we have already verified bytes contains enough space for `num_instruction` headers.
            let header = unsafe { Self::read_v1_header(bytes, &mut header_offset) };

            let payload_len = usize::from(header.num_accounts)
                .checked_add(usize::from(header.data_len))
                .ok_or(TransactionViewError::ParseError)?;

            total_payload_len = total_payload_len
                .checked_add(payload_len)
                .ok_or(TransactionViewError::ParseError)?;
        }

        check_remaining(bytes, *offset, total_payload_len)?;
        *offset = offset.wrapping_add(total_payload_len);

        Ok(Self::V1 {
            num_instructions: u16::from(num_instructions),
            headers_offset,
            payloads_offset,
        })
    }

    /// # Safety
    /// `bytes[*offset..*offset + size_of::<V1InstructionHeader>()]` must be valid.
    #[inline(always)]
    unsafe fn read_v1_header(bytes: &[u8], offset: &mut usize) -> V1InstructionHeader {
        let mut header: V1InstructionHeader = unsafe { unchecked_copy_value(bytes, *offset) };
        *offset = offset.wrapping_add(core::mem::size_of::<V1InstructionHeader>());
        header.data_len = u16::from_le(header.data_len);
        header
    }

    #[inline(always)]
    pub(crate) fn num_instructions(&self) -> u16 {
        match self {
            Self::LegacyAndV0 {
                num_instructions, ..
            } => *num_instructions,
            Self::V1 {
                num_instructions, ..
            } => *num_instructions,
        }
    }

    #[inline(always)]
    pub(crate) fn iter<'a>(&'a self, bytes: &'a [u8]) -> InstructionsIterator<'a> {
        match self {
            Self::LegacyAndV0 {
                num_instructions,
                offset,
                frames,
            } => InstructionsIterator::LegacyAndV0 {
                bytes,
                offset: *offset as usize,
                index: 0,
                num_instructions: *num_instructions,
                frames,
            },
            Self::V1 {
                num_instructions,
                headers_offset,
                payloads_offset,
            } => InstructionsIterator::V1 {
                bytes,
                index: 0,
                num_instructions: *num_instructions,
                headers_offset: *headers_offset as usize,
                payloads_offset: *payloads_offset as usize,
            },
        }
    }
}

#[derive(Clone)]
pub enum InstructionsIterator<'a> {
    LegacyAndV0 {
        bytes: &'a [u8],
        offset: usize,
        num_instructions: u16,
        index: u16,
        frames: &'a [LegacyAndV0InstructionFrame],
    },
    V1 {
        bytes: &'a [u8],
        index: u16,
        num_instructions: u16,
        headers_offset: usize,
        payloads_offset: usize,
    },
}

impl<'a> Iterator for InstructionsIterator<'a> {
    type Item = SVMInstruction<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::LegacyAndV0 {
                bytes,
                offset,
                index,
                num_instructions,
                frames,
            } => {
                if *index >= *num_instructions {
                    return None;
                }

                let LegacyAndV0InstructionFrame {
                    num_accounts,
                    num_accounts_len,
                    data_len,
                    data_len_len,
                } = frames[usize::from(*index)];

                *index = index.wrapping_add(1);

                Some(unsafe {
                    for_legacy_and_v0(
                        bytes,
                        offset,
                        num_accounts,
                        num_accounts_len,
                        data_len,
                        data_len_len,
                    )
                })
            }
            Self::V1 {
                bytes,
                index,
                num_instructions,
                headers_offset,
                payloads_offset,
            } => {
                if *index >= *num_instructions {
                    return None;
                }

                let header = unsafe { InstructionsFrame::read_v1_header(bytes, headers_offset) };
                *index = index.wrapping_add(1);

                Some(unsafe {
                    for_v1(
                        bytes,
                        payloads_offset,
                        header.program_id_index,
                        u16::from(header.num_accounts),
                        header.data_len,
                    )
                })
            }
        }
    }
}

/// Builds SNVInstruction from legacy/v0 pre-validated frame metadata.
///
/// # Safety
/// The caller must ensure that:
/// - `offset` points to the beginning of a serialized legacy/v0 instruction
///   in `bytes`.
/// - `num_accounts_len` and `data_len_len` are the exact encoded lengths of the
///   compact-u16 account-count and data-length fields for that instruction.
/// - `num_accounts` and `data_len` exactly match the serialized instruction at
///   `offset`.
/// - The byte ranges implied by those values are fully in bounds of `bytes`.
///
/// These invariants are expected to have been established by the initial
/// instruction frame parsing. Violating them may cause out-of-bounds unchecked
/// reads and undefined behavior.
#[inline(always)]
unsafe fn for_legacy_and_v0<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
    num_accounts: u16,
    num_accounts_len: u8,
    data_len: u16,
    data_len_len: u8,
) -> SVMInstruction<'a> {
    // Each instruction has 3 pieces:
    // 1. Program ID index (u8)
    // 2. Accounts indexes ([u8])
    // 3. Data ([u8])

    // Read the program ID index.
    // SAFETY: Offset and length checks have been done in the initial parsing.
    let program_id_index = unsafe { unchecked_read_byte(bytes, offset) };

    // Move offset to accounts offset - do not re-parse u16.
    *offset = offset.wrapping_add(usize::from(num_accounts_len));
    const _: () = assert!(core::mem::align_of::<u8>() == 1, "u8 alignment");
    // SAFETY:
    // - The offset is checked to be valid in the byte slice.
    // - The alignment of u8 is 1.
    // - The slice length is checked to be valid.
    // - `u8` cannot be improperly initialized.
    // - Offset and length checks have been done in the initial parsing.
    let accounts = unsafe { unchecked_read_slice_data::<u8>(bytes, offset, num_accounts) };

    // Move offset to accounts offset - do not re-parse u16.
    *offset = offset.wrapping_add(usize::from(data_len_len));
    const _: () = assert!(core::mem::align_of::<u8>() == 1, "u8 alignment");
    // SAFETY:
    // - The offset is checked to be valid in the byte slice.
    // - The alignment of u8 is 1.
    // - The slice length is checked to be valid.
    // - `u8` cannot be improperly initialized.
    // - Offset and length checks have been done in the initial parsing.
    let data = unsafe { unchecked_read_slice_data::<u8>(bytes, offset, data_len) };

    SVMInstruction {
        program_id_index,
        accounts,
        data,
    }
}

/// Builds SMVInstruction from v1 pre-validated frame metadata.
///
/// # Safety
/// The caller must ensure that:
///
/// - `payload_offset` points to the beginning of this instruction’s payload
///   (i.e. the first account index byte) within `bytes`.
/// - `num_accounts` and `data_len` exactly match the instruction header that
///   was previously parsed for this instruction.
/// - The byte range
///   `payload_offset .. payload_offset + num_accounts + data_len`
///   lies entirely within `bytes`.
/// - `bytes` has not been mutated since the initial parsing that produced
///   the instruction frames.
///
/// These invariants are expected to have been established during the initial
/// tx-v1 instruction parsing phase, where header and payload bounds were
/// validated together.
///
/// Violating any of these conditions may result in out-of-bounds unchecked
/// reads and thus undefined behavior.
#[inline(always)]
unsafe fn for_v1<'a>(
    bytes: &'a [u8],
    payloads_offset: &mut usize,
    program_id_index: u8,
    num_accounts: u16,
    data_len: u16,
) -> SVMInstruction<'a> {
    // SAFETY:
    // - The offset is checked to be valid in the byte slice.
    // - The alignment of u8 is 1.
    // - The slice length is checked to be valid.
    // - `u8` cannot be improperly initialized.
    // - Offset and length checks have been done in the initial parsing.
    let accounts = unsafe { unchecked_read_slice_data::<u8>(bytes, payloads_offset, num_accounts) };
    // SAFETY:
    // - The offset is checked to be valid in the byte slice.
    // - The alignment of u8 is 1.
    // - The slice length is checked to be valid.
    // - `u8` cannot be improperly initialized.
    // - Offset and length checks have been done in the initial parsing.
    let data = unsafe { unchecked_read_slice_data::<u8>(bytes, payloads_offset, data_len) };

    SVMInstruction {
        program_id_index,
        accounts,
        data,
    }
}

impl ExactSizeIterator for InstructionsIterator<'_> {
    fn len(&self) -> usize {
        match self {
            Self::LegacyAndV0 {
                num_instructions,
                index,
                ..
            } => usize::from(num_instructions.wrapping_sub(*index)),
            Self::V1 {
                num_instructions,
                index,
                ..
            } => usize::from(num_instructions.wrapping_sub(*index)),
        }
    }
}

impl Debug for InstructionsIterator<'_> {
    fn fmt(&self, f: &mut Formatter) -> core::fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_message::compiled_instruction::CompiledInstruction,
        solana_short_vec::ShortVec,
    };

    impl InstructionsFrame {
        fn offset(&self) -> u16 {
            match self {
                Self::LegacyAndV0 { offset, .. } => *offset,
                Self::V1 { headers_offset, .. } => *headers_offset,
            }
        }
    }

    #[test]
    fn test_zero_instructions() {
        let bytes = bincode::serialize(&ShortVec(Vec::<CompiledInstruction>::new())).unwrap();
        let mut offset = 0;
        let instructions_frame =
            InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).unwrap();

        assert_eq!(instructions_frame.num_instructions(), 0);
        assert_eq!(instructions_frame.offset(), 1);
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn test_num_instructions_too_high() {
        let mut bytes = bincode::serialize(&ShortVec(vec![CompiledInstruction {
            program_id_index: 0,
            accounts: vec![],
            data: vec![],
        }]))
        .unwrap();
        // modify the number of instructions to be too high
        bytes[0] = 0x02;
        let mut offset = 0;
        assert!(InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_single_instruction() {
        let bytes = bincode::serialize(&ShortVec(vec![CompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2, 3],
            data: vec![4, 5, 6, 7, 8, 9, 10],
        }]))
        .unwrap();
        let mut offset = 0;
        let instructions_frame =
            InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).unwrap();
        assert_eq!(instructions_frame.num_instructions(), 1);
        assert_eq!(instructions_frame.offset(), 1);
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn test_multiple_instructions() {
        let bytes = bincode::serialize(&ShortVec(vec![
            CompiledInstruction {
                program_id_index: 0,
                accounts: vec![1, 2, 3],
                data: vec![4, 5, 6, 7, 8, 9, 10],
            },
            CompiledInstruction {
                program_id_index: 1,
                accounts: vec![4, 5, 6],
                data: vec![7, 8, 9, 10, 11, 12, 13],
            },
        ]))
        .unwrap();
        let mut offset = 0;
        let instructions_frame =
            InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).unwrap();
        assert_eq!(instructions_frame.num_instructions(), 2);
        assert_eq!(instructions_frame.offset(), 1);
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn test_invalid_instruction_accounts_vec() {
        let mut bytes = bincode::serialize(&ShortVec(vec![CompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2, 3],
            data: vec![4, 5, 6, 7, 8, 9, 10],
        }]))
        .unwrap();

        // modify the number of accounts to be too high
        bytes[2] = 127;

        let mut offset = 0;
        assert!(InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_invalid_instruction_data_vec() {
        let mut bytes = bincode::serialize(&ShortVec(vec![CompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2, 3],
            data: vec![4, 5, 6, 7, 8, 9, 10],
        }]))
        .unwrap();

        // modify the number of data bytes to be too high
        bytes[6] = 127;

        let mut offset = 0;
        assert!(InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_txv1_instructions_iterator() {
        let message = solana_message::v1::Message {
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 0,
                    accounts: vec![1, 2, 3],
                    data: vec![4, 5, 6, 7, 8, 9, 10],
                },
                CompiledInstruction {
                    program_id_index: 10,
                    accounts: vec![11, 12],
                    data: vec![13, 14, 15, 16, 17, 18, 19, 20],
                },
            ],
            ..solana_message::v1::Message::default()
        };

        let serialized = solana_message::v1::serialize(&message);

        let mut offset = 41; // instruction headers starts at offset 41 for no config, 0 addresses, per spec
        let instructions_frame =
            InstructionsFrame::try_new_for_v1(&serialized, &mut offset, 2).unwrap();

        let mut iter = instructions_frame.iter(&serialized);
        assert_eq!(
            iter.next(),
            Some(SVMInstruction {
                program_id_index: 0,
                accounts: &[1, 2, 3],
                data: &[4, 5, 6, 7, 8, 9, 10]
            })
        );
        assert_eq!(
            iter.next(),
            Some(SVMInstruction {
                program_id_index: 10,
                accounts: &[11, 12],
                data: &[13, 14, 15, 16, 17, 18, 19, 20]
            })
        );
        assert_eq!(iter.next(), None);
    }

    fn short_u16_1(x: u8) -> Vec<u8> {
        vec![x]
    }

    // short_vec / compact-u16 encoding for 128..=16383 style values
    fn short_u16_2(x: u16) -> Vec<u8> {
        assert!(x >= 128);
        vec![((x & 0x7f) as u8) | 0x80, (x >> 7) as u8]
    }

    #[test]
    fn test_try_new_legacy_single_instruction() {
        // num_instructions = 1
        // instruction:
        //   program_id_index = 7
        //   num_accounts = 2
        //   accounts = [3, 4]
        //   data_len = 3
        //   data = [9, 8, 7]
        let bytes = vec![
            1, // num_instructions
            7, // program_id_index
            2, // num_accounts
            3, 4, // account indexes
            3, // data_len
            9, 8, 7, // data
        ];

        let mut offset = 0;
        let frame = InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).unwrap();

        assert_eq!(offset, bytes.len());

        match frame {
            InstructionsFrame::LegacyAndV0 {
                num_instructions,
                offset,
                frames,
            } => {
                assert_eq!(num_instructions, 1);
                assert_eq!(offset, 1);
                assert_eq!(frames.len(), 1);
                let ix = &frames[0];
                assert_eq!(ix.num_accounts, 2);
                assert_eq!(ix.data_len, 3);
                assert_eq!(ix.num_accounts_len, 1);
                assert_eq!(ix.data_len_len, 1);
            }
            _ => panic!("expected legacy/v0 repr"),
        }
    }

    #[test]
    fn test_try_new_legacy_two_byte_lengths() {
        let num_accounts = 128u16;
        let data_len = 130u16;

        let mut bytes = Vec::new();
        bytes.push(1); // num_instructions
        bytes.push(42); // program_id_index
        bytes.extend_from_slice(&short_u16_2(num_accounts));
        bytes.extend(std::iter::repeat_n(5u8, num_accounts as usize));
        bytes.extend_from_slice(&short_u16_2(data_len));
        bytes.extend(std::iter::repeat_n(9u8, data_len as usize));

        let mut offset = 0;
        let frame = InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).unwrap();

        assert_eq!(offset, bytes.len());

        match frame {
            InstructionsFrame::LegacyAndV0 {
                num_instructions,
                offset,
                frames,
            } => {
                assert_eq!(num_instructions, 1);
                assert_eq!(offset, 1);
                assert_eq!(frames.len(), 1);
                let ix = &frames[0];
                assert_eq!(ix.num_accounts, num_accounts);
                assert_eq!(ix.data_len, data_len);

                assert_eq!(ix.num_accounts_len, 2);
                assert_eq!(ix.data_len_len, 2);
            }
            _ => panic!("expected legacy/v0 repr"),
        }
    }

    #[test]
    fn test_try_new_for_v1_single_instruction() {
        // one v1 instruction
        // header:
        //   program_id_index = 9
        //   num_accounts = 2
        //   data_len = 3
        // payload:
        //   accounts = [10, 11]
        //   data = [1, 2, 3]
        let bytes = vec![
            9, // program_id_index
            2, // num_accounts
            3, 0, // data_len (u16 LE)
            10, 11, // payload accounts
            1, 2, 3, // payload data
        ];

        let mut offset = 0;
        let frame = InstructionsFrame::try_new_for_v1(&bytes, &mut offset, 1).unwrap();

        assert_eq!(offset, bytes.len());

        match frame {
            InstructionsFrame::V1 {
                num_instructions,
                headers_offset,
                payloads_offset,
            } => {
                assert_eq!(num_instructions, 1);
                assert_eq!(headers_offset, 0);
                assert_eq!(payloads_offset, 4);
                let hdr = unsafe { InstructionsFrame::read_v1_header(&bytes, &mut 0) };
                assert_eq!(hdr.program_id_index, 9);
                assert_eq!(hdr.num_accounts, 2);
                assert_eq!(hdr.data_len, 3);
            }
            _ => panic!("expected v1 repr"),
        }
    }

    #[test]
    fn test_try_new_for_v1_two_instructions() {
        // headers:
        //   ix0: pid=1, accounts=2, data_len=1
        //   ix1: pid=7, accounts=1, data_len=2
        //
        // payloads:
        //   ix0: [20, 21] [99]
        //   ix1: [42] [5, 6]
        let bytes = vec![
            // header 0
            1, 2, 1, 0, // header 1
            7, 1, 2, 0, // payload 0
            20, 21, 99, // payload 1
            42, 5, 6,
        ];

        let mut offset = 0;
        let frame = InstructionsFrame::try_new_for_v1(&bytes, &mut offset, 2).unwrap();

        assert_eq!(offset, bytes.len());
        match frame {
            InstructionsFrame::V1 {
                num_instructions,
                headers_offset,
                payloads_offset,
            } => {
                assert_eq!(num_instructions, 2);
                assert_eq!(headers_offset, 0);
                assert_eq!(payloads_offset, 8);
                let hdr = unsafe { InstructionsFrame::read_v1_header(&bytes, &mut 0) };
                assert_eq!(hdr.program_id_index, 1);
                assert_eq!(hdr.num_accounts, 2);
                assert_eq!(hdr.data_len, 1);
                let hdr = unsafe { InstructionsFrame::read_v1_header(&bytes, &mut 4) };
                assert_eq!(hdr.program_id_index, 7);
                assert_eq!(hdr.num_accounts, 1);
                assert_eq!(hdr.data_len, 2);
            }
            _ => panic!("expected v1 repr"),
        }
    }

    #[test]
    fn test_try_new_for_v1_truncated_header_fails() {
        // num_instructions = 1, but only 3 header bytes instead of 4
        let bytes = vec![
            9, // program_id_index
            2, // num_accounts
            3, // incomplete data_len
        ];

        let mut offset = 0;
        assert!(InstructionsFrame::try_new_for_v1(&bytes, &mut offset, 1).is_err());
    }

    #[test]
    fn test_try_new_for_v1_truncated_payload_fails() {
        // header says payload len = 2 + 3 = 5, but only 4 bytes provided
        let bytes = vec![
            9, 2, 3, 0, // header
            10, 11, 1, 2, // truncated payload
        ];

        let mut offset = 0;
        assert!(InstructionsFrame::try_new_for_v1(&bytes, &mut offset, 1).is_err());
    }

    #[test]
    fn test_try_new_legacy_truncated_payload_fails() {
        // data_len says 3, only 2 bytes provided
        let bytes = vec![
            1, // num_instructions
            7, // program_id_index
            1, // num_accounts
            9, // account idx
            3, // data_len
            1, 2, // truncated data
        ];

        let mut offset = 0;
        assert!(InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_try_new_for_v1_zero_instructions() {
        let bytes = vec![];
        let mut offset = 0;

        let frame = InstructionsFrame::try_new_for_v1(&bytes, &mut offset, 0).unwrap();
        assert_eq!(offset, 0);
        match frame {
            InstructionsFrame::V1 {
                num_instructions,
                headers_offset,
                payloads_offset,
            } => {
                assert_eq!(num_instructions, 0);
                assert_eq!(headers_offset, 0);
                assert_eq!(payloads_offset, 0);
            }
            _ => panic!("expected v1 repr"),
        }
    }

    #[test]
    fn data_len_max_header_fails_parse() {
        // header: pid=1, accounts=1, data_len=65535
        let bytes = vec![1, 1, 0xff, 0xff];
        let mut offset = 0;
        assert_eq!(
            InstructionsFrame::try_new_for_v1(&bytes, &mut offset, 1)
                .err()
                .unwrap(),
            TransactionViewError::ParseError
        );
    }

    #[test]
    fn test_try_new_legacy_zero_instructions() {
        let bytes = short_u16_1(0);
        let mut offset = 0;

        let frame = InstructionsFrame::try_new_for_legacy_and_v0(&bytes, &mut offset).unwrap();
        assert_eq!(offset, 1);

        match frame {
            InstructionsFrame::LegacyAndV0 {
                num_instructions,
                offset,
                frames,
            } => {
                assert_eq!(num_instructions, 0);
                assert_eq!(offset, 1);
                assert!(frames.is_empty());
            }
            _ => panic!("expected legacy/v0 repr"),
        }
    }
}
