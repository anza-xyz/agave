use {
    rand::{Rng, rng},
    solana_pubkey::{PUBKEY_BYTES, Pubkey},
    std::{fmt, num::NonZeroUsize},
};

type ReadBytesType = u32;

// The bin calculator assumes a pubkey is 32 bytes, so enforce that here.
const _: () = assert!(PUBKEY_BYTES == 32);

const BITS_PER_BYTE: usize = u8::BITS as usize;
const _: () = assert!(BITS_PER_BYTE == 8);

/// The maximum number of bins we can support.
///
/// This is based on the number of bytes we read in `read_bytes()`.
///
/// Basically, if we read four bytes (32 bits) from the pubkey as it's "hash",
/// and can have a maximum bit-offset of seven,
/// then the maximum number of bins, as pow2, is 32 - 7 == 25.
///
/// 2^25 bins is over 33 million bins, and that should be more than enough.
/// If we do ever need more, then changing ReadBytesType to u64 gets us 2^57 bins.
///
/// To get the real number, do `pow2(MAX_BINS_POW2)`.
const MAX_BINS_POW2: usize = (size_of::<ReadBytesType>() - 1) * BITS_PER_BYTE + 1;
const _: () = assert!(MAX_BINS_POW2 == 25);

/// The maximum offset we can support.
///
/// This is based on the maximum number of bins.
/// Take the number of bits in a pubkey (256) and subtract the number of bits for max bins (25).
const MAX_OFFSET: usize = PUBKEY_BYTES * BITS_PER_BYTE - MAX_BINS_POW2;
const _: () = assert!(MAX_OFFSET == 231);

/// The bin calculator's byte_offset must be <= this value.
///
/// This ensures we can read enough bytes from the pubkey to calculate the bin.
const MAX_BYTE_OFFSET: usize = PUBKEY_BYTES - size_of::<ReadBytesType>();
const _: () = assert!(MAX_BYTE_OFFSET == 28);

/// Used to calculate which bin a pubkey maps to.
///
/// This struct may be cloned, and will retain the same pubkey -> bin results.
///
/// To instantiate, use `PubkeyBinCalculatorBuilder::with_bins(num_bins)`.
#[derive(Clone)]
pub struct PubkeyBinCalculator {
    /// Offset, in bytes, where to begin reading from the pubkey.
    byte_offset: usize,
    /// Offset, in bits, into the read_bytes() for the start of the bin.
    bit_offset: usize,
    /// Mask to calculate the bin, based on the number of bins.
    mask: ReadBytesType,
}

impl PubkeyBinCalculator {
    /// Calculates the bin that `pubkey` maps to.
    #[inline]
    pub fn bin_from_pubkey(&self, pubkey: &Pubkey) -> usize {
        // This debug assert checks that enough was read from the pubkey to calculate the bin.
        // The number of bits for num_bins + number of bits for bit_offset
        // *must* be <= number of bits read from the pubkey.
        debug_assert!((self.mask + 1).ilog2() + self.bit_offset as u32 <= ReadBytesType::BITS);
        let bytes = self.read_bytes(pubkey);
        let bin = (bytes >> self.bit_offset) & self.mask;
        // SAFETY: bin is a u32, which can fit in a usize
        // (Unfortunately the trait `std::convert::From<u32>` is not implemented for `usize`)
        bin as usize
    }

    /// Read the bytes from `pubkey` needed to calculate the bin.
    #[inline]
    fn read_bytes(&self, pubkey: &Pubkey) -> ReadBytesType {
        debug_assert!(self.byte_offset <= MAX_BYTE_OFFSET);
        let ptr = pubkey.as_array().as_ptr();
        // Because we know we're reading valid bytes within the pubkey, unsafe is used
        // to avoid bounds checks that would occur if reading via slice indexing.
        //
        // SAFETY:
        //
        // - `byte_offset` was checked at construction to be in range to read a ReadBytesType.
        //
        // add() is safe:
        // - `byte_offset` can fit in an isize.
        // - `byte_offset` is in-range of `pubkey`.
        //
        // read_unaligned() is safe:
        // - the ptr being read is valid
        //   - the ptr came from `pubkey`.
        //   - the memory range being read is entirely contained within the
        //     bounds of the allocation (this was checked above by `add()`).
        // - the value of the type being read (ReadBytesType) is valid
        //   - the memory of `pubkey` has been initialized
        unsafe {
            ptr.add(self.byte_offset)
                .cast::<ReadBytesType>()
                .read_unaligned()
        }
    }
}

impl fmt::Debug for PubkeyBinCalculator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkeyBinCalculator")
            .field("num_bins", &(self.mask + 1))
            .field(
                "offset",
                &(self.byte_offset * BITS_PER_BYTE + self.bit_offset),
            )
            .finish()
    }
}

/// Used to build unique instances of `PubkeyBinCalculator'.
#[derive(Debug)]
pub struct PubkeyBinCalculatorBuilder;

impl PubkeyBinCalculatorBuilder {
    /// Builds a `PubkeyBinCalculator` with `num_bins`.
    ///
    /// The returned bin calculator will produce *unique* mappings
    /// compared to other bin calculators!
    ///
    /// # Panics
    ///
    /// This function will panic if the following conditions are not met:
    /// * `num_bins` must be a power of two
    /// * `num_bins` must be <= 2^25
    pub fn with_bins(num_bins: NonZeroUsize) -> PubkeyBinCalculator {
        // Skip the beginning and end of the pubkey range, which is the most common to grind.
        const SKIP: usize = 16;
        let offset = rng().random_range(SKIP..=(MAX_OFFSET - SKIP));
        Self::with_bins_and_offset(num_bins, offset)
    }

    /// Builds a `PubkeyBinCalculator` with `num_bins` and `offset`.
    ///
    /// The `offset` is used to instantiate a specific PubkeyHasher for the bin calculator.
    /// Prefer `with_bins()` whenever possible.
    ///
    /// The returned bin calculator will produce *identical* mappings
    /// compared to other bin calculators with the same num_bins and offset.
    ///
    /// # Panics
    ///
    /// This function will panic if the following conditions are not met:
    /// * `num_bins` must be a power of two
    /// * `num_bins` must be <= 2^25
    /// * `offset` must be <= 231
    pub fn with_bins_and_offset(num_bins: NonZeroUsize, offset: usize) -> PubkeyBinCalculator {
        assert!(
            offset <= MAX_OFFSET,
            "offset must be <= {MAX_OFFSET} (actual: {offset})",
        );
        assert!(
            num_bins.is_power_of_two(),
            "num_bins must be a power of two (actual: {num_bins})",
        );
        assert!(
            num_bins.get() <= (1 << MAX_BINS_POW2),
            "num_bins must be <= 2^{MAX_BINS_POW2} (actual: {num_bins})",
        );
        let byte_offset = offset / BITS_PER_BYTE;
        let bit_offset = offset - (byte_offset * BITS_PER_BYTE);
        // SAFETY: We just checked that num_bins is <= MAX_BINS, which is less than u32::MAX.
        let num_bins_mask = u32::try_from(num_bins.get() - 1).unwrap();
        PubkeyBinCalculator {
            byte_offset,
            bit_offset,
            mask: num_bins_mask,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure that bin calculation is deterministic.
    #[test]
    fn test_bin_from_pubkey_is_deterministic() {
        for num_bins in [1 << 10, 1 << 14, 1 << 19, 1 << MAX_BINS_POW2] {
            let bin_calculator1 =
                PubkeyBinCalculatorBuilder::with_bins(NonZeroUsize::new(num_bins).unwrap());
            // second bin calculator that exercies cloning
            let bin_calculator2 = bin_calculator1.clone();
            for i_pubkey in 0..1_000 {
                let pubkey = solana_pubkey::new_rand();
                let expected_bin = bin_calculator1.bin_from_pubkey(&pubkey);
                for i_calculation in 0..10 {
                    let actual_bin = bin_calculator1.bin_from_pubkey(&pubkey);
                    assert_eq!(
                        actual_bin, expected_bin,
                        "num_bins: {num_bins}, i_pubkey: {i_pubkey}, i_calculation: \
                         {i_calculation}, pubkey: {pubkey}",
                    );
                }
                assert_eq!(expected_bin, bin_calculator2.bin_from_pubkey(&pubkey));
            }
        }
    }

    /// Ensure that bin calculators from *different* builders produce different hashes.
    #[test]
    fn test_builders_produces_unique_instances() {
        let num_bins = NonZeroUsize::new(1).unwrap();
        let bin_calculator1 = PubkeyBinCalculatorBuilder::with_bins(num_bins);
        let bin_calculator2 = loop {
            let bc2 = PubkeyBinCalculatorBuilder::with_bins(num_bins);
            if bc2.byte_offset != bin_calculator1.byte_offset {
                break bc2;
            }
        };
        let pubkey = solana_pubkey::new_rand();
        assert_ne!(
            bin_calculator1.read_bytes(&pubkey),
            bin_calculator2.read_bytes(&pubkey),
        );
    }

    /// Ensure that bin calculators from different builders, but with the
    /// same num_bins and offset, produce *identical* hashes.
    #[test]
    fn test_builders_with_same_offset_produce_identical_instances() {
        let num_bins = NonZeroUsize::new(1 << 20).unwrap();
        let offset = 123;
        let bin_calculator1 = PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, offset);
        let bin_calculator2 = PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, offset);
        let pubkey = solana_pubkey::new_rand();
        assert_eq!(
            bin_calculator1.read_bytes(&pubkey),
            bin_calculator2.read_bytes(&pubkey),
        );
        assert_eq!(
            bin_calculator1.bin_from_pubkey(&pubkey),
            bin_calculator2.bin_from_pubkey(&pubkey),
        );
    }

    /// Ensure all valid number of bins can be used.
    #[test]
    fn test_all_valid_bins() {
        let pubkey = Pubkey::new_unique();
        for num_bins_pow2 in 0..=MAX_BINS_POW2 {
            let num_bins = NonZeroUsize::new(1 << num_bins_pow2).unwrap();
            let bin_calculator = PubkeyBinCalculatorBuilder::with_bins(num_bins);
            let bin = bin_calculator.bin_from_pubkey(&pubkey);
            // wrap in a block box to ensure the compiler doesn't elide the bin calculation
            std::hint::black_box(bin);
        }
    }

    /// Ensure all valid offsets can be used.
    #[test]
    fn test_all_valid_offsets() {
        let pubkey = Pubkey::new_unique();
        let num_bins = NonZeroUsize::new(1).unwrap();
        for offset in 0..=MAX_OFFSET {
            let bin_calculator = PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, offset);
            let bin = bin_calculator.bin_from_pubkey(&pubkey);
            // wrap in a block box to ensure the compiler doesn't elide the bin calculation
            std::hint::black_box(bin);
        }
    }

    /// Ensure non-power-of-two number of bins is not allowed.
    #[test]
    #[should_panic(expected = "num_bins must be a power of two")]
    fn test_num_bins_not_power_of_two_should_panic() {
        let num_bins = NonZeroUsize::new(3).unwrap();
        PubkeyBinCalculatorBuilder::with_bins(num_bins);
    }

    /// Ensure number of bins is in range.
    #[test]
    #[should_panic(expected = "num_bins must be <= 2^25")]
    fn test_num_bins_too_large_should_panic() {
        let num_bins = NonZeroUsize::new(1 << (MAX_BINS_POW2 + 1)).unwrap();
        PubkeyBinCalculatorBuilder::with_bins(num_bins);
    }

    /// Ensure offset is in range.
    #[test]
    #[should_panic(expected = "offset must be <= 231")]
    fn test_bad_offset_should_panic() {
        let num_bins = NonZeroUsize::new(1).unwrap();
        PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, MAX_OFFSET + 1);
    }

    /// Ensure enough is read from the pubkey to calculate the bin.
    #[test]
    #[should_panic(
        expected = "(self.mask + 1).ilog2() + self.bit_offset as u32 <= ReadBytesType::BITS"
    )]
    fn test_not_enough_read_from_pubkey_should_panic() {
        let num_bits_read_from_pubkey = ReadBytesType::BITS;
        let num_bins_bits = 13;
        let bad_bit_offset = num_bits_read_from_pubkey - num_bins_bits + 1;
        let bin_calculator = PubkeyBinCalculator {
            byte_offset: 0,
            bit_offset: bad_bit_offset.try_into().unwrap(),
            mask: (1 << num_bins_bits) - 1,
        };
        bin_calculator.bin_from_pubkey(&Pubkey::default());
    }
}
