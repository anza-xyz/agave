use {
    crate::{
        bytes::{
            advance_offset_for_array, check_remaining, optimized_read_compressed_u16, read_byte,
            unchecked_read_byte, unchecked_read_slice_data,
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
        let headers_len = 4usize.wrapping_mul(num_instructions as usize);

        check_remaining(bytes, *offset, headers_len)?;

        let mut header_offset = *offset;
        *offset = offset.wrapping_add(headers_len);

        let payloads_offset = *offset as u16;

        // Validate the entire payload region without allocating.
        for _ in 0..num_instructions {
            let header = Self::read_v1_header(bytes, &mut header_offset)?;

            let payload_len = u16::from(header.num_accounts)
                .checked_add(header.data_len)
                .ok_or(TransactionViewError::ParseError)?;

            advance_offset_for_array::<u8>(bytes, offset, payload_len)?;
        }

        Ok(Self::V1 {
            num_instructions: num_instructions as u16,
            headers_offset,
            payloads_offset,
        })
    }

    #[inline(always)]
    fn read_v1_header(bytes: &[u8], offset: &mut usize) -> Result<V1InstructionHeader> {
        let program_id_index = read_byte(bytes, offset)?;
        let num_accounts = read_byte(bytes, offset)?;
        let data_len_lo = read_byte(bytes, offset)?;
        let data_len_hi = read_byte(bytes, offset)?;

        Ok(V1InstructionHeader {
            program_id_index,
            num_accounts,
            data_len: u16::from_le_bytes([data_len_lo, data_len_hi]),
        })
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

    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn offset(&self) -> u16 {
        match self {
            Self::LegacyAndV0 { offset, .. } => *offset,
            Self::V1 { headers_offset, .. } => *headers_offset,
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

                let mut header_cursor = *headers_offset;
                let program_id_index = unsafe { unchecked_read_byte(bytes, &mut header_cursor) };
                let num_accounts = unsafe { unchecked_read_byte(bytes, &mut header_cursor) };
                let data_len_lo = unsafe { unchecked_read_byte(bytes, &mut header_cursor) };
                let data_len_hi = unsafe { unchecked_read_byte(bytes, &mut header_cursor) };
                let data_len = u16::from_le_bytes([data_len_lo, data_len_hi]);

                *headers_offset = header_cursor;
                *index = index.wrapping_add(1);

                Some(unsafe {
                    for_v1(
                        bytes,
                        payloads_offset,
                        program_id_index,
                        num_accounts as u16,
                        data_len,
                    )
                })
            }
        }
    }
}

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

#[inline(always)]
unsafe fn for_v1<'a>(
    bytes: &'a [u8],
    payloads_offset: &mut usize,
    program_id_index: u8,
    num_accounts: u16,
    data_len: u16,
) -> SVMInstruction<'a> {
    let accounts = unsafe { unchecked_read_slice_data::<u8>(bytes, payloads_offset, num_accounts) };
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

    #[test]
    fn test_zero_instructions() {
        let bytes = bincode::serialize(&ShortVec(Vec::<CompiledInstruction>::new())).unwrap();
        let mut offset = 0;
        let instructions_frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();

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
        let instructions_frame = InstructionsFrame::try_new(&bytes, &mut offset).unwrap();
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
}
