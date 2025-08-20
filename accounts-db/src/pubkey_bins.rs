use solana_pubkey::Pubkey;

#[derive(Clone, Copy, Debug)]
pub struct PubkeyBinCalculator24 {
    // how many bits from the first 3 bytes to shift away to ignore when calculating bin
    shift_bits: u32,
}

impl PubkeyBinCalculator24 {
    const MAX_BITS: u32 = 24;

    const fn num_bits<T>() -> usize {
        std::mem::size_of::<T>() * 8
    }

    pub(crate) fn log_2(x: u32) -> u32 {
        assert!(x > 0);
        Self::num_bits::<u32>() as u32 - x.leading_zeros() - 1
    }

    pub fn new(bins: usize) -> Self {
        assert!(bins > 0);
        let max_plus_1 = 1 << Self::MAX_BITS;
        assert!(bins <= max_plus_1);
        assert!(bins.is_power_of_two());
        let bits = Self::log_2(bins as u32);
        Self {
            shift_bits: Self::MAX_BITS - bits,
        }
    }

    pub fn bins(&self) -> usize {
        1 << (Self::MAX_BITS - self.shift_bits)
    }

    #[inline]
    pub fn bin_from_pubkey(&self, pubkey: &Pubkey) -> usize {
        let as_ref = pubkey.as_ref();
        (((as_ref[0] as usize) << 16) | ((as_ref[1] as usize) << 8) | (as_ref[2] as usize))
            >> self.shift_bits
    }

    pub(crate) fn lowest_pubkey_from_bin(&self, mut bin: usize) -> Pubkey {
        assert!(bin < self.bins());
        bin <<= self.shift_bits;
        let mut pubkey = Pubkey::from([0; 32]);
        pubkey.as_mut()[0] = ((bin / 256 / 256) & 0xff) as u8;
        pubkey.as_mut()[1] = ((bin / 256) & 0xff) as u8;
        pubkey.as_mut()[2] = (bin & 0xff) as u8;
        pubkey
    }

    pub(crate) fn highest_pubkey_from_bin(&self, mut bin: usize) -> Pubkey {
        assert!(bin < self.bins());
        let mask = (1 << self.shift_bits) - 1;
        bin <<= self.shift_bits;
        bin |= mask;
        let mut pubkey = Pubkey::from([0xff; 32]);
        pubkey.as_mut()[0] = ((bin / 256 / 256) & 0xff) as u8;
        pubkey.as_mut()[1] = ((bin / 256) & 0xff) as u8;
        pubkey.as_mut()[2] = (bin & 0xff) as u8;
        pubkey
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_pubkey_bins_log2() {
        assert_eq!(PubkeyBinCalculator24::num_bits::<u8>(), 8);
        assert_eq!(PubkeyBinCalculator24::num_bits::<u32>(), 32);
        for i in 0..32 {
            assert_eq!(PubkeyBinCalculator24::log_2(2u32.pow(i)), i);
        }
    }

    #[test]
    fn test_pubkey_lowest_highest_bin4() {
        let calc = PubkeyBinCalculator24::new(4);

        // bin 0
        let expected_lowest = Pubkey::from([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ]);
        let expected_highest = Pubkey::from([
            0x3F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        assert_eq!(calc.lowest_pubkey_from_bin(0), expected_lowest);
        assert_eq!(calc.highest_pubkey_from_bin(0), expected_highest);

        // bin 1
        let expected_lowest = Pubkey::from([
            0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ]);
        let expected_highest = Pubkey::from([
            0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        assert_eq!(calc.lowest_pubkey_from_bin(1), expected_lowest,);
        assert_eq!(calc.highest_pubkey_from_bin(1), expected_highest);

        // bin 2
        let expected_lowest = Pubkey::from([
            0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ]);
        let expected_highest = Pubkey::from([
            0xBF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        assert_eq!(calc.lowest_pubkey_from_bin(2), expected_lowest);
        assert_eq!(calc.highest_pubkey_from_bin(2), expected_highest);

        // bin 3
        let expected_lowest = Pubkey::from([
            0xC0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ]);
        let expected_highest = Pubkey::from([
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        assert_eq!(calc.lowest_pubkey_from_bin(3), expected_lowest);
        assert_eq!(calc.highest_pubkey_from_bin(3), expected_highest);
    }

    #[test]
    fn test_pubkey_bins() {
        for i in 0..=24 {
            let bins = 2u32.pow(i);
            let calc = PubkeyBinCalculator24::new(bins as usize);
            assert_eq!(calc.shift_bits, 24 - i, "i: {i}");
            for bin in 0..bins {
                assert_eq!(
                    bin as usize,
                    calc.bin_from_pubkey(&calc.lowest_pubkey_from_bin(bin as usize))
                );

                assert_eq!(
                    bin as usize,
                    calc.bin_from_pubkey(&calc.highest_pubkey_from_bin(bin as usize))
                );

                assert_eq!(calc.bins(), bins as usize);
            }
        }
    }

    #[test]
    fn test_pubkey_bins_pubkeys() {
        let mut pk = Pubkey::from([0; 32]);
        for i in 0..=8 {
            let bins = 2usize.pow(i);
            let calc = PubkeyBinCalculator24::new(bins);

            let shift_bits = calc.shift_bits - 16; // we are only dealing with first byte

            pk.as_mut()[0] = 0;
            assert_eq!(0, calc.bin_from_pubkey(&pk));
            pk.as_mut()[0] = 0xff;
            assert_eq!(bins - 1, calc.bin_from_pubkey(&pk));

            for bin in 0..bins {
                pk.as_mut()[0] = (bin << shift_bits) as u8;
                assert_eq!(
                    bin,
                    calc.bin_from_pubkey(&pk),
                    "bin: {}/{}, shift_bits: {}, val: {}",
                    bin,
                    bins,
                    shift_bits,
                    pk.as_ref()[0]
                );
                if bin > 0 {
                    pk.as_mut()[0] = ((bin << shift_bits) - 1) as u8;
                    assert_eq!(bin - 1, calc.bin_from_pubkey(&pk));
                }
            }
        }

        for i in 9..=16 {
            let mut pk = Pubkey::from([0; 32]);
            let bins = 2usize.pow(i);
            let calc = PubkeyBinCalculator24::new(bins);

            let shift_bits = calc.shift_bits - 8;

            pk.as_mut()[1] = 0;
            assert_eq!(0, calc.bin_from_pubkey(&pk));
            pk.as_mut()[0] = 0xff;
            pk.as_mut()[1] = 0xff;
            assert_eq!(bins - 1, calc.bin_from_pubkey(&pk));

            let mut pk = Pubkey::from([0; 32]);
            for bin in 0..bins {
                let mut target = (bin << shift_bits) as u16;
                pk.as_mut()[0] = (target / 256) as u8;
                pk.as_mut()[1] = (target % 256) as u8;
                assert_eq!(
                    bin,
                    calc.bin_from_pubkey(&pk),
                    "bin: {}/{}, shift_bits: {}, val: {}",
                    bin,
                    bins,
                    shift_bits,
                    pk.as_ref()[0]
                );
                if bin > 0 {
                    target -= 1;
                    pk.as_mut()[0] = (target / 256) as u8;
                    pk.as_mut()[1] = (target % 256) as u8;
                    assert_eq!(bin - 1, calc.bin_from_pubkey(&pk));
                }
            }
        }

        for i in 17..=24 {
            let mut pk = Pubkey::from([0; 32]);
            let bins = 2usize.pow(i);
            let calc = PubkeyBinCalculator24::new(bins);

            let shift_bits = calc.shift_bits;

            pk.as_mut()[1] = 0;
            assert_eq!(0, calc.bin_from_pubkey(&pk));
            pk.as_mut()[0] = 0xff;
            pk.as_mut()[1] = 0xff;
            pk.as_mut()[2] = 0xff;
            assert_eq!(bins - 1, calc.bin_from_pubkey(&pk));

            let mut pk = Pubkey::from([0; 32]);
            for bin in 0..bins {
                let mut target = (bin << shift_bits) as u32;
                pk.as_mut()[0] = (target / 256 / 256) as u8;
                pk.as_mut()[1] = ((target / 256) % 256) as u8;
                pk.as_mut()[2] = (target % 256) as u8;
                assert_eq!(
                    bin,
                    calc.bin_from_pubkey(&pk),
                    "bin: {}/{}, shift_bits: {}, val: {:?}",
                    bin,
                    bins,
                    shift_bits,
                    &pk.as_ref()[0..3],
                );
                if bin > 0 {
                    target -= 1;
                    pk.as_mut()[0] = (target / 256 / 256) as u8;
                    pk.as_mut()[1] = ((target / 256) % 256) as u8;
                    pk.as_mut()[2] = (target % 256) as u8;
                    assert_eq!(bin - 1, calc.bin_from_pubkey(&pk));
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "bins.is_power_of_two()")]
    fn test_pubkey_bins_illegal_bins3() {
        PubkeyBinCalculator24::new(3);
    }

    #[test]
    #[should_panic(expected = "bins <= max_plus_1")]
    fn test_pubkey_bins_illegal_bins2() {
        PubkeyBinCalculator24::new(65536 * 256 + 1);
    }
    #[test]
    #[should_panic(expected = "bins > 0")]
    fn test_pubkey_bins_illegal_bins() {
        PubkeyBinCalculator24::new(0);
    }
}
