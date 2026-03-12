use {
    crate::{
        bytes::{
            advance_offset_for_array, check_remaining, optimized_read_compressed_u16, read_byte,
            read_slice_data, unchecked_read_byte, unchecked_read_slice_data,
        },
        result::Result,
    },
    core::fmt::{Debug, Formatter},
    solana_svm_transaction::instruction::SVMInstruction,
};

/// Contains metadata about the instructions in a transaction packet.
#[derive(Debug, Default)]
pub(crate) struct InstructionsFrame {
    /// The number of instructions in the transaction.
    pub(crate) num_instructions: u16,
    /// The offset to the first instruction in the transaction.
    pub(crate) offset: u16,
    pub(crate) frames: Vec<InstructionFrame>,
}

#[derive(Debug)]
pub(crate) struct InstructionFrame {
    /// Per-instruction anchor offset.
    ///
    /// - legacy/v0: start of the serialized CompiledInstruction
    /// - v1: start of the 4-byte InstructionHeader entry
    offset: u16,

    program_id_index: u8,
    num_accounts: u16, // NOTE: v1 spec-ed it as u8
    data_len: u16,

    /// how to interpret frame between different transaction version
    repr: InstructionFrameRepr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstructionFrameRepr {
    /// Legacy and v0:
    /// [program_id_index][short_vec accounts][accounts...][short_vec data][data...]
    LegacyAndV0 {
        num_accounts_len: u8, // either 1 or 2
        data_len_len: u8,     // either 1 or 2
    },

    /// V1:
    /// header lives at `offset`, payload lives elsewhere.
    V1 {
        /// Start of this instruction's payload:
        /// [account_indexes...][instruction_data...]
        payload_offset: u16,
    },
}

#[derive(Debug, Clone, Copy)]
struct V1InstructionHeader {
    offset: u16,
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
    pub(crate) fn try_new(bytes: &[u8], offset: &mut usize) -> Result<Self> {
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
            let instruction_offset = *offset as u16;

            // Each instruction has 3 pieces:
            // 1. Program ID index (u8)
            // 2. Accounts indexes ([u8])
            // 3. Data ([u8])

            // Read the program ID index.
            let program_id_index = read_byte(bytes, offset)?;

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

            frames.push(InstructionFrame {
                offset: instruction_offset,
                program_id_index,
                num_accounts,
                data_len,
                repr: InstructionFrameRepr::LegacyAndV0 {
                    num_accounts_len,
                    data_len_len,
                },
            });
        }

        Ok(Self {
            num_instructions,
            offset: instructions_offset,
            frames,
        })
    }

    #[inline(always)]
    pub(crate) fn try_new_for_v1(
        bytes: &[u8],
        offset: &mut usize,
        num_instructions: u8, // NOTE: v1 spec-ed it out as u8
    ) -> Result<Self> {
        let headers_offset = *offset as u16;

        // advance instruction headers
        let headers = Self::parse_v1_instruction_headers(bytes, offset, num_instructions)?;
        // then advance instruction payloads
        let frames = Self::build_v1_instruction_frames(bytes, &headers, offset)?;

        Ok(Self {
            num_instructions: num_instructions.into(),
            offset: headers_offset,
            frames,
        })
    }

    fn parse_v1_instruction_headers(
        bytes: &[u8],
        offset: &mut usize,
        num_instructions: u8,
    ) -> Result<Vec<V1InstructionHeader>> {
        let mut headers = Vec::with_capacity(num_instructions as usize);

        for _ in 0..(num_instructions as usize) {
            let header_offset = *offset as u16;
            let program_id_index = read_byte(bytes, offset)?;
            let num_instruction_accounts = read_byte(bytes, offset)?;
            let num_instruction_data_bytes = u16::from_le_bytes(
                unsafe { read_slice_data::<u8>(bytes, offset, 2) }?
                    .try_into()
                    .map_err(|_| crate::result::TransactionViewError::ParseError)?,
            );

            headers.push(V1InstructionHeader {
                offset: header_offset,
                program_id_index,
                num_accounts: num_instruction_accounts,
                data_len: num_instruction_data_bytes,
            });
        }

        Ok(headers)
    }

    fn build_v1_instruction_frames(
        bytes: &[u8],
        headers: &[V1InstructionHeader],
        offset: &mut usize,
    ) -> Result<Vec<InstructionFrame>> {
        let mut frames = Vec::with_capacity(headers.len());

        for header in headers {
            let payload_len = u16::from(header.num_accounts).wrapping_add(header.data_len);

            frames.push(InstructionFrame {
                offset: header.offset,
                num_accounts: header.num_accounts as u16,
                data_len: header.data_len,
                program_id_index: header.program_id_index,
                repr: InstructionFrameRepr::V1 {
                    payload_offset: *offset as u16,
                },
            });

            // advance offset
            advance_offset_for_array::<u8>(bytes, offset, payload_len)?;
        }

        Ok(frames)
    }
}

#[derive(Clone)]
pub struct InstructionsIterator<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) offset: usize,
    pub(crate) num_instructions: u16,
    pub(crate) index: u16,
    pub(crate) frames: &'a [InstructionFrame],
}

impl<'a> Iterator for InstructionsIterator<'a> {
    type Item = SVMInstruction<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.num_instructions {
            let InstructionFrame {
                offset: _,
                program_id_index,
                num_accounts,
                data_len,
                repr,
            } = self.frames[usize::from(self.index)];

            self.index = self.index.wrapping_add(1);

            match repr {
                InstructionFrameRepr::LegacyAndV0 {
                    num_accounts_len,
                    data_len_len,
                } => Some(self.for_legacy_and_v0(
                    program_id_index,
                    num_accounts,
                    num_accounts_len,
                    data_len,
                    data_len_len,
                )),
                InstructionFrameRepr::V1 { payload_offset } => {
                    Some(self.for_v1(program_id_index, num_accounts, data_len, payload_offset))
                }
            }
        } else {
            None
        }
    }
}

impl<'a> InstructionsIterator<'a> {
    fn for_legacy_and_v0(
        &mut self,
        program_id_index: u8,
        num_accounts: u16,
        num_accounts_len: u8,
        data_len: u16,
        data_len_len: u8,
    ) -> <InstructionsIterator<'a> as Iterator>::Item {
        // Each instruction has 3 pieces:
        // 1. Program ID index (u8)
        // 2. Accounts indexes ([u8])
        // 3. Data ([u8])

        // Read the program ID index.
        // SAFETY: Offset and length checks have been done in the initial parsing.
        let _unused_program_id_index = unsafe { unchecked_read_byte(self.bytes, &mut self.offset) };

        // Move offset to accounts offset - do not re-parse u16.
        self.offset = self.offset.wrapping_add(usize::from(num_accounts_len));
        const _: () = assert!(core::mem::align_of::<u8>() == 1, "u8 alignment");
        // SAFETY:
        // - The offset is checked to be valid in the byte slice.
        // - The alignment of u8 is 1.
        // - The slice length is checked to be valid.
        // - `u8` cannot be improperly initialized.
        // - Offset and length checks have been done in the initial parsing.
        let accounts =
            unsafe { unchecked_read_slice_data::<u8>(self.bytes, &mut self.offset, num_accounts) };

        // Move offset to accounts offset - do not re-parse u16.
        self.offset = self.offset.wrapping_add(usize::from(data_len_len));
        const _: () = assert!(core::mem::align_of::<u8>() == 1, "u8 alignment");
        // SAFETY:
        // - The offset is checked to be valid in the byte slice.
        // - The alignment of u8 is 1.
        // - The slice length is checked to be valid.
        // - `u8` cannot be improperly initialized.
        // - Offset and length checks have been done in the initial parsing.
        let data =
            unsafe { unchecked_read_slice_data::<u8>(self.bytes, &mut self.offset, data_len) };

        SVMInstruction {
            program_id_index,
            accounts,
            data,
        }
    }

    fn for_v1(
        &mut self,
        program_id_index: u8,
        num_accounts: u16,
        data_len: u16,
        payload_offset: u16,
    ) -> <InstructionsIterator<'a> as Iterator>::Item {
        self.offset = payload_offset as usize;
        let accounts =
            unsafe { unchecked_read_slice_data::<u8>(self.bytes, &mut self.offset, num_accounts) };

        let data =
            unsafe { unchecked_read_slice_data::<u8>(self.bytes, &mut self.offset, data_len) };

        SVMInstruction {
            program_id_index,
            accounts,
            data,
        }
    }
}

impl ExactSizeIterator for InstructionsIterator<'_> {
    fn len(&self) -> usize {
        usize::from(self.num_instructions.wrapping_sub(self.index))
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

    #[test]
    fn test_zero_instructions() {
        let bytes = bincode::serialize(&ShortVec(Vec::<CompiledInstruction>::new())).unwrap();
        let mut offset = 0;
        let instructions_frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();

        assert_eq!(instructions_frame.num_instructions, 0);
        assert_eq!(instructions_frame.offset, 1);
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
        assert!(InstructionsFrame::try_new(&bytes, &mut offset).is_err());
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
        let instructions_frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();
        assert_eq!(instructions_frame.num_instructions, 1);
        assert_eq!(instructions_frame.offset, 1);
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
        let instructions_frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();
        assert_eq!(instructions_frame.num_instructions, 2);
        assert_eq!(instructions_frame.offset, 1);
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
        assert!(InstructionsFrame::try_new(&bytes, &mut offset).is_err());
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
        assert!(InstructionsFrame::try_new(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_txv1_instruction_data() {
        let mut message = solana_message::v1::Message::default();
        message.instructions = vec![CompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2, 3],
            data: vec![4, 5, 6, 7, 8, 9, 10],
        }];

        let serialized = solana_message::v1::serialize(&message);

        let mut offset = 41; // instruction headers starts at offset 41 for no config, 0 addresses, per spec
        let instructions_frame =
            InstructionsFrame::try_new_for_v1(&serialized, &mut offset, 1).unwrap();
        assert_eq!(instructions_frame.num_instructions, 1);
        assert_eq!(instructions_frame.frames.len(), 1);
        let instruction_frame = &instructions_frame.frames[0];
        assert_eq!(instruction_frame.offset, 41);
        assert_eq!(instruction_frame.program_id_index, 0);
        assert_eq!(instruction_frame.num_accounts, 3);
        assert_eq!(instruction_frame.data_len, 7);
        assert!(matches!(
            instruction_frame.repr,
            InstructionFrameRepr::V1 { payload_offset: 45 }
        ));
    }

    #[test]
    fn test_txv1_instructions_iterator() {
        let mut message = solana_message::v1::Message::default();
        message.instructions = vec![
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
        ];

        let serialized = solana_message::v1::serialize(&message);

        let mut offset = 41; // instruction headers starts at offset 41 for no config, 0 addresses, per spec
        let instructions_frame =
            InstructionsFrame::try_new_for_v1(&serialized, &mut offset, 2).unwrap();

        let mut iter = InstructionsIterator {
            bytes: &serialized,
            offset,
            num_instructions: instructions_frame.num_instructions,
            index: 0,
            frames: &instructions_frame.frames,
        };

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
        let frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();

        assert_eq!(frame.num_instructions, 1);
        assert_eq!(frame.offset, 1);
        assert_eq!(frame.frames.len(), 1);
        assert_eq!(offset, bytes.len());

        let ix = &frame.frames[0];
        assert_eq!(ix.offset, 1);
        assert_eq!(ix.program_id_index, 7);
        assert_eq!(ix.num_accounts, 2);
        assert_eq!(ix.data_len, 3);

        match ix.repr {
            InstructionFrameRepr::LegacyAndV0 {
                num_accounts_len,
                data_len_len,
            } => {
                assert_eq!(num_accounts_len, 1);
                assert_eq!(data_len_len, 1);
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
        let frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();

        assert_eq!(frame.num_instructions, 1);
        assert_eq!(frame.offset, 1);
        assert_eq!(frame.frames.len(), 1);
        assert_eq!(offset, bytes.len());

        let ix = &frame.frames[0];
        assert_eq!(ix.offset, 1);
        assert_eq!(ix.program_id_index, 42);
        assert_eq!(ix.num_accounts, num_accounts);
        assert_eq!(ix.data_len, data_len);

        match ix.repr {
            InstructionFrameRepr::LegacyAndV0 {
                num_accounts_len,
                data_len_len,
            } => {
                assert_eq!(num_accounts_len, 2);
                assert_eq!(data_len_len, 2);
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

        assert_eq!(frame.num_instructions, 1);
        assert_eq!(frame.offset, 0);
        assert_eq!(frame.frames.len(), 1);
        assert_eq!(offset, bytes.len());

        let ix = &frame.frames[0];
        assert_eq!(ix.offset, 0);
        assert_eq!(ix.program_id_index, 9);
        assert_eq!(ix.num_accounts, 2);
        assert_eq!(ix.data_len, 3);

        match ix.repr {
            InstructionFrameRepr::V1 { payload_offset } => {
                assert_eq!(payload_offset, 4);
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

        assert_eq!(frame.num_instructions, 2);
        assert_eq!(frame.offset, 0);
        assert_eq!(frame.frames.len(), 2);
        assert_eq!(offset, bytes.len());

        let ix0 = &frame.frames[0];
        assert_eq!(ix0.offset, 0);
        assert_eq!(ix0.program_id_index, 1);
        assert_eq!(ix0.num_accounts, 2);
        assert_eq!(ix0.data_len, 1);
        match ix0.repr {
            InstructionFrameRepr::V1 { payload_offset } => assert_eq!(payload_offset, 8),
            _ => panic!("expected v1 repr"),
        }

        let ix1 = &frame.frames[1];
        assert_eq!(ix1.offset, 4);
        assert_eq!(ix1.program_id_index, 7);
        assert_eq!(ix1.num_accounts, 1);
        assert_eq!(ix1.data_len, 2);
        match ix1.repr {
            InstructionFrameRepr::V1 { payload_offset } => assert_eq!(payload_offset, 11),
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
        assert!(InstructionsFrame::try_new(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_try_new_for_v1_zero_instructions() {
        let bytes = vec![];
        let mut offset = 0;

        let frame = InstructionsFrame::try_new_for_v1(&bytes, &mut offset, 0).unwrap();
        assert_eq!(frame.num_instructions, 0);
        assert_eq!(frame.offset, 0);
        assert!(frame.frames.is_empty());
        assert_eq!(offset, 0);
    }

    #[test]
    fn test_parse_v1_headers_advances_then_payloads_advance() {
        // This specifically checks the contract that:
        // - parse_v1_instruction_headers advances offset by 4 * num_instructions
        // - build_v1_instruction_frames advances by total payload length
        let bytes = vec![
            // headers (2 * 4 bytes)
            3, 1, 2, 0, // pid=3, accounts=1, data_len=2
            4, 2, 1, 0, // pid=4, accounts=2, data_len=1
            // payloads
            99, 7, 8, // 1 + 2
            10, 11, 12, // 2 + 1
        ];

        let mut offset = 0;
        let headers =
            InstructionsFrame::parse_v1_instruction_headers(&bytes, &mut offset, 2).unwrap();
        assert_eq!(offset, 8);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].offset, 0);
        assert_eq!(headers[1].offset, 4);

        let frames =
            InstructionsFrame::build_v1_instruction_frames(&bytes, &headers, &mut offset).unwrap();
        assert_eq!(offset, bytes.len());
        assert_eq!(frames.len(), 2);

        match frames[0].repr {
            InstructionFrameRepr::V1 { payload_offset } => assert_eq!(payload_offset, 8),
            _ => panic!("expected v1 repr"),
        }
        match frames[1].repr {
            InstructionFrameRepr::V1 { payload_offset } => assert_eq!(payload_offset, 11),
            _ => panic!("expected v1 repr"),
        }
    }

    #[test]
    fn test_try_new_legacy_zero_instructions() {
        let bytes = short_u16_1(0);
        let mut offset = 0;

        let frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();
        assert_eq!(frame.num_instructions, 0);
        assert_eq!(frame.offset, 1);
        assert!(frame.frames.is_empty());
        assert_eq!(offset, 1);
    }
}
