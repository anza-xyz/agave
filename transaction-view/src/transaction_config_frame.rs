use {
    crate::{
        bytes::{advance_offset_for_array, check_remaining},
        result::{Result, TransactionViewError},
    },
    solana_program_runtime::execution_budget::MIN_HEAP_FRAME_BYTES,
};

/// Metadata for accessing the tx-v1 transaction config section.
///
/// This frame is a permanent part of `TransactionFrame`, but it is only
/// applicable to tx-v1. For legacy and v0 transactions, use
/// `TransactionConfigFrame::not_applicable()`.
///
/// Layout, per SIMD-0385:
///   TransactionConfigMask (u32 LE)
///   ...
///   ConfigValues [[u8; 4]]   // len = popcount(mask)
///
/// Notes:
/// - `offset == 0` is reserved to mean "not applicable" (legacy/v0).
/// - Parsed tx-v1 config frames should always have `offset != 0`.
/// - `try_new()` parses only the 4-byte mask at the current offset.
/// - `with_values_offset()` is called later, once the parser has advanced
///   past the addresses section and knows where `ConfigValues` begins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TransactionConfigFrame {
    /// Offset of the 4-byte TransactionConfigMask.
    ///
    /// `0` means "not applicable" (legacy/v0).
    pub(crate) mask_offset: u16,

    /// Decoded TransactionConfigMask.
    pub(crate) mask: u32,

    /// Offset of the first ConfigValues word.
    ///
    /// `0` means "not applicable" (legacy/v0)
    pub(crate) values_offset: u16,

    /// Number of 4-byte words in ConfigValues.
    pub(crate) num_values: u8,

    /// priority fee in lamports
    pub(crate) priority_fee_lamports: u64,
    pub(crate) compute_unit_limit: u32,
    pub(crate) loaded_accounts_data_size_limit: u32,
    pub(crate) requested_heap_size: u32,
}

#[allow(dead_code)]
impl TransactionConfigFrame {
    pub(crate) const MASK_SIZE: usize = core::mem::size_of::<u32>();
    pub(crate) const CONFIG_VALUE_SIZE: usize = 4;

    /// Sentinel for legacy / v0 transactions.
    #[inline(always)]
    pub(crate) const fn not_applicable() -> Self {
        Self {
            mask_offset: 0,
            mask: 0,
            values_offset: 0,
            num_values: 0,
            priority_fee_lamports: 0,
            compute_unit_limit: 0,
            loaded_accounts_data_size_limit: 0,
            requested_heap_size: MIN_HEAP_FRAME_BYTES,
        }
    }

    /// Returns true if this frame represents a tx-v1 transaction config.
    #[inline(always)]
    pub(crate) const fn is_present(&self) -> bool {
        self.mask_offset != 0
    }

    #[inline(always)]
    pub(crate) const fn is_not_applicable(&self) -> bool {
        !self.is_present()
    }

    /// Config Mask has been successfully parsed before advancing to `ConfigValues`
    /// region; Now can try to create TransactionConfigFrame by parsing values.
    #[inline(always)]
    pub(crate) fn try_new(
        bytes: &[u8],
        mask_offset: usize,
        mask: u32,
        offset: &mut usize,
    ) -> Result<Self> {
        assert!(mask_offset > 0, "txv1 mask offset must be greater than 0");

        Self::sanitize_mask(mask)?;
        let num_values = mask.count_ones() as u8;
        let mask_offset =
            u16::try_from(mask_offset).map_err(|_| TransactionViewError::SanitizeError)?;
        let values_offset =
            u16::try_from(*offset).map_err(|_| TransactionViewError::SanitizeError)?;
        // Validate that the config-values region is in bounds.
        check_remaining(
            bytes,
            values_offset as usize,
            (num_values as usize).wrapping_mul(Self::CONFIG_VALUE_SIZE),
        )?;
        // advance offset
        advance_offset_for_array::<u32>(bytes, offset, num_values as u16)?;

        // create frame with default config values
        let mut frame = Self {
            mask_offset,
            mask,
            values_offset,
            num_values,
            priority_fee_lamports: 0,
            compute_unit_limit: 0,
            loaded_accounts_data_size_limit: 0,
            requested_heap_size: MIN_HEAP_FRAME_BYTES,
        };

        // parse and read actual config values if set
        frame.priority_fee_lamports(bytes)?;
        frame.compute_unit_limit(bytes)?;
        frame.loaded_accounts_data_size_limit(bytes)?;
        frame.requested_heap_size(bytes)?;

        Ok(frame)
    }

    /// Validate mask semantics.
    ///
    /// Check unknown / reserved bits are not used; And
    /// Bits 0 and 1 together encode one logical 8-byte priority-fee field,
    /// so they must either both be set or both be clear.
    #[inline(always)]
    fn sanitize_mask(mask: u32) -> Result<()> {
        const ALLOWED_TRANSACTION_CONFIG_MASK: u32 = 0b1_1111;

        // Reject unknown / reserved bits
        if mask & !ALLOWED_TRANSACTION_CONFIG_MASK != 0 {
            return Err(TransactionViewError::SanitizeError);
        }

        let bit0 = Self::has_bit(mask, 0);
        let bit1 = Self::has_bit(mask, 1);
        if bit0 ^ bit1 {
            return Err(TransactionViewError::SanitizeError);
        }

        Ok(())
    }

    #[inline(always)]
    fn has_bit(mask: u32, bit: u8) -> bool {
        bit < 32 && ((mask >> bit) & 1) != 0
    }

    /// Bits 0 and 1 form one logical 8-byte priority-fee field.
    #[inline(always)]
    fn priority_fee_lamports(&mut self, bytes: &[u8]) -> Result<()> {
        let bit0 = Self::has_bit(self.mask, 0);
        let bit1 = Self::has_bit(self.mask, 1);

        if !bit0 && !bit1 {
            return Ok(());
        }
        if bit0 ^ bit1 {
            return Err(TransactionViewError::SanitizeError);
        }

        let lo_offset = self
            .word_offset(0)
            .ok_or(TransactionViewError::ParseError)?;
        let hi_offset = self
            .word_offset(1)
            .ok_or(TransactionViewError::ParseError)?;

        let lo = bytes
            .get(lo_offset..lo_offset.wrapping_add(Self::CONFIG_VALUE_SIZE))
            .ok_or(TransactionViewError::ParseError)?;
        let hi = bytes
            .get(hi_offset..hi_offset.wrapping_add(Self::CONFIG_VALUE_SIZE))
            .ok_or(TransactionViewError::ParseError)?;

        let mut out = [0u8; 8];
        out[..Self::CONFIG_VALUE_SIZE].copy_from_slice(lo);
        out[Self::CONFIG_VALUE_SIZE..].copy_from_slice(hi);
        self.priority_fee_lamports = u64::from_le_bytes(out);
        Ok(())
    }

    #[inline(always)]
    fn compute_unit_limit(&mut self, bytes: &[u8]) -> Result<()> {
        if Self::has_bit(self.mask, 2) {
            self.compute_unit_limit = self
                .read_u32_for_bit(bytes, 2)
                .ok_or(TransactionViewError::ParseError)?;
        }
        Ok(())
    }

    #[inline(always)]
    fn loaded_accounts_data_size_limit(&mut self, bytes: &[u8]) -> Result<()> {
        if Self::has_bit(self.mask, 3) {
            self.loaded_accounts_data_size_limit = self
                .read_u32_for_bit(bytes, 3)
                .ok_or(TransactionViewError::ParseError)?;
        }
        Ok(())
    }

    #[inline(always)]
    fn requested_heap_size(&mut self, bytes: &[u8]) -> Result<()> {
        if Self::has_bit(self.mask, 4) {
            self.requested_heap_size = self
                .read_u32_for_bit(bytes, 4)
                .ok_or(TransactionViewError::ParseError)?;
        }
        Ok(())
    }

    /// Return the packed word index for a given set bit. Eg: counts
    /// bits set below `bit`.
    ///
    /// Example:
    ///   mask = 0b0001_1100
    ///   bit 2 -> 0
    ///   bit 3 -> 1
    ///   bit 4 -> 2
    #[inline(always)]
    pub(crate) fn word_index_for_bit(&self, bit: u8) -> Option<u8> {
        if self.is_not_applicable() || !Self::has_bit(self.mask, bit) {
            return None;
        }

        let mask_before_bit = (1u32 << bit).wrapping_sub(1);
        Some((self.mask & mask_before_bit).count_ones() as u8)
    }

    #[inline(always)]
    fn word_offset(&self, bit: u8) -> Option<usize> {
        let word_index = usize::from(self.word_index_for_bit(bit)?);
        Some(
            usize::from(self.values_offset)
                .wrapping_add(word_index.wrapping_mul(Self::CONFIG_VALUE_SIZE)),
        )
    }

    #[inline(always)]
    pub(crate) fn read_u32_for_bit(&self, bytes: &[u8], bit: u8) -> Option<u32> {
        let offset = self.word_offset(bit)?;
        let word = bytes.get(offset..offset.wrapping_add(4))?;
        Some(u32::from_le_bytes(word.try_into().ok()?))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TransactionConfigView<'a> {
    pub(crate) transaction_config_frame: &'a TransactionConfigFrame,
}

impl<'a> TransactionConfigView<'a> {
    #[inline(always)]
    pub fn priority_fee_lamports(&self) -> u64 {
        self.transaction_config_frame.priority_fee_lamports
    }

    #[inline(always)]
    pub fn compute_unit_limit(&self) -> u32 {
        self.transaction_config_frame.compute_unit_limit
    }

    #[inline(always)]
    pub fn loaded_accounts_data_size_limit(&self) -> u32 {
        self.transaction_config_frame
            .loaded_accounts_data_size_limit
    }

    #[inline(always)]
    pub fn requested_heap_size(&self) -> u32 {
        self.transaction_config_frame.requested_heap_size
    }

    #[inline(always)]
    pub fn mask(&self) -> u32 {
        self.transaction_config_frame.mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32le(x: u32) -> [u8; 4] {
        x.to_le_bytes()
    }

    fn u64_words_le(x: u64) -> ([u8; 4], [u8; 4]) {
        let bytes = x.to_le_bytes();
        (
            [bytes[0], bytes[1], bytes[2], bytes[3]],
            [bytes[4], bytes[5], bytes[6], bytes[7]],
        )
    }

    #[test]
    fn test_not_applicable_defaults() {
        let frame = TransactionConfigFrame::not_applicable();

        assert!(frame.is_not_applicable());
        assert!(!frame.is_present());
        assert_eq!(TransactionConfigFrame::sanitize_mask(frame.mask), Ok(()));
        assert_eq!(frame.priority_fee_lamports, 0);
        assert_eq!(frame.compute_unit_limit, 0);
        assert_eq!(frame.loaded_accounts_data_size_limit, 0);
        assert_eq!(frame.requested_heap_size, MIN_HEAP_FRAME_BYTES);
    }

    #[test]
    fn test_try_new_zero_mask_is_present() {
        let mask = 0u32;
        let bytes = mask.to_le_bytes();
        let mut buf = vec![0u8; 5];
        let mask_offset = 5;
        buf.extend_from_slice(&bytes);

        let mut offset = buf.len();
        let frame = TransactionConfigFrame::try_new(&buf, mask_offset, mask, &mut offset).unwrap();

        assert!(frame.is_present());
        assert_eq!(frame.mask_offset, 5);
        assert_eq!(frame.mask, 0);
        assert_eq!(frame.num_values, 0);
        assert_eq!(offset, 9);
    }

    #[test]
    fn test_try_new_invalid_priority_fee_half_set_low_bit() {
        let mask = 0b00001u32;
        let bytes = mask.to_le_bytes();
        let mut buf = vec![0u8; 5];
        let mask_offset = 5;
        buf.extend_from_slice(&bytes);
        let mut offset = 5;

        assert_eq!(
            TransactionConfigFrame::try_new(&buf, mask_offset, mask, &mut offset),
            Err(TransactionViewError::SanitizeError)
        );
    }

    #[test]
    fn test_try_new_invalid_priority_fee_half_set_high_bit() {
        let mask = 0b00010u32;
        let bytes = mask.to_le_bytes();
        let mut buf = vec![0u8; 5];
        let mask_offset = 5;
        buf.extend_from_slice(&bytes);
        let mut offset = 5;

        assert_eq!(
            TransactionConfigFrame::try_new(&buf, mask_offset, mask, &mut offset),
            Err(TransactionViewError::SanitizeError)
        );
    }

    #[test]
    fn test_with_values_offset_checks_remaining() {
        // bits 0,1,2 => 3 words => 12 bytes needed
        let mask = 0b00111u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[7u8; 3]);
        let mask_offset = bytes.len();
        bytes.extend_from_slice(&mask.to_le_bytes());

        let mut offset = bytes.len();
        assert_eq!(
            TransactionConfigFrame::try_new(&bytes, mask_offset, mask, &mut offset),
            Err(TransactionViewError::ParseError)
        );
    }

    #[test]
    fn test_read_defaults_when_bits_unset() {
        let mask = 0u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[1u8; 2]);
        let mask_offset = bytes.len();
        bytes.extend_from_slice(&mask.to_le_bytes());

        let mut offset = bytes.len();
        let frame =
            TransactionConfigFrame::try_new(&bytes, mask_offset, mask, &mut offset).unwrap();

        assert_eq!(frame.priority_fee_lamports, 0);
        assert_eq!(frame.compute_unit_limit, 0);
        assert_eq!(frame.loaded_accounts_data_size_limit, 0);
        assert_eq!(frame.requested_heap_size, MIN_HEAP_FRAME_BYTES);
    }

    #[test]
    fn test_priority_fee_only() {
        let mask = 0b00011u32;
        let fee = 123_456_789u64;
        let (lo, hi) = u64_words_le(fee);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[9u8; 4]);
        let mask_offset = bytes.len();
        bytes.extend_from_slice(&mask.to_le_bytes());
        let values_offset = bytes.len();
        bytes.extend_from_slice(&lo);
        bytes.extend_from_slice(&hi);

        let mut offset = values_offset;
        let frame = TransactionConfigFrame::try_new(&bytes, mask_offset, mask, &mut offset)
            .inspect(|_| assert_eq!(offset, bytes.len()))
            .unwrap();

        assert!(frame.is_present());
        assert_eq!(frame.num_values, 2);
        assert_eq!(frame.priority_fee_lamports, fee);
        assert_eq!(frame.compute_unit_limit, 0);
        assert_eq!(frame.requested_heap_size, MIN_HEAP_FRAME_BYTES);
    }

    #[test]
    fn test_all_initial_fields_present() {
        // bits 0,1,2,3,4 => priority fee + cu + loaded data size + heap size
        let mask = 0b1_1111u32;
        let fee = 99u64;
        let cu = 1_400_000u32;
        let loaded = 64_000u32;
        let heap = 64 * 1024u32;

        let (fee_lo, fee_hi) = u64_words_le(fee);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[9u8; 7]);
        let mask_offset = bytes.len();
        bytes.extend_from_slice(&mask.to_le_bytes());
        let values_offset = bytes.len();
        bytes.extend_from_slice(&fee_lo);
        bytes.extend_from_slice(&fee_hi);
        bytes.extend_from_slice(&u32le(cu));
        bytes.extend_from_slice(&u32le(loaded));
        bytes.extend_from_slice(&u32le(heap));

        let mut offset = values_offset;
        let frame = TransactionConfigFrame::try_new(&bytes, mask_offset, mask, &mut offset)
            .inspect(|_| assert_eq!(offset, bytes.len()))
            .unwrap();

        assert!(frame.is_present());
        assert_eq!(frame.num_values, 5);
        assert_eq!(frame.priority_fee_lamports, fee);
        assert_eq!(frame.compute_unit_limit, cu);
        assert_eq!(frame.loaded_accounts_data_size_limit, loaded);
        assert_eq!(frame.requested_heap_size, heap);
    }

    #[test]
    fn test_sparse_bits_word_indexing() {
        // bits 2 and 4 only
        let mask = 0b10100u32;
        let cu = 777u32;
        let heap = 48 * 1024u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[1u8; 3]);
        let mask_offset = bytes.len();
        bytes.extend_from_slice(&mask.to_le_bytes());
        let values_offset = bytes.len();
        bytes.extend_from_slice(&u32le(cu)); // bit 2 -> word 0
        bytes.extend_from_slice(&u32le(heap)); // bit 4 -> word 1

        let mut offset = values_offset;
        let frame = TransactionConfigFrame::try_new(&bytes, mask_offset, mask, &mut offset)
            .inspect(|_| assert_eq!(offset, bytes.len()))
            .unwrap();

        assert_eq!(frame.word_index_for_bit(2), Some(0));
        assert_eq!(frame.word_index_for_bit(4), Some(1));
        assert_eq!(frame.word_index_for_bit(3), None);

        assert_eq!(frame.compute_unit_limit, cu);
        assert_eq!(frame.loaded_accounts_data_size_limit, 0);
        assert_eq!(frame.requested_heap_size, heap);
    }

    #[test]
    fn test_truncated_priority_fee_values() {
        let mask = 0b00011u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[5u8; 2]);
        let mask_offset = bytes.len();
        bytes.extend_from_slice(&mask.to_le_bytes());
        let values_offset = bytes.len();
        bytes.extend_from_slice(&[1, 2, 3, 4]); // only one word present

        let mut offset = values_offset;
        assert_eq!(
            TransactionConfigFrame::try_new(&bytes, mask_offset, mask, &mut offset),
            Err(TransactionViewError::ParseError)
        );
    }

    #[test]
    fn test_sanitize_values_region_after_attach() {
        let mask = 0b00100u32; // one value
        let value = 123u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[8u8; 6]);
        let mask_offset = bytes.len();
        bytes.extend_from_slice(&mask.to_le_bytes());
        let values_offset = bytes.len();
        bytes.extend_from_slice(&value.to_le_bytes());
        let eof = bytes.len();

        let mut offset = values_offset;
        let frame = TransactionConfigFrame::try_new(&bytes, mask_offset, mask, &mut offset)
            .inspect(|_| assert_eq!(offset, eof))
            .unwrap();

        assert_eq!(frame.compute_unit_limit, value);
    }
}
