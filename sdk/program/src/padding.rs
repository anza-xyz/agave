//! Types supporting padding with zeroed bytes.

/// A byte that is always zeroed. Most operations are no-ops.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ZeroedPaddingByte {
    #[default]
    Zero = 0,
}

/// A set of zeroed padding bytes.
pub type ZeroedPadding<const N: usize> = [ZeroedPaddingByte; N];
