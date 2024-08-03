//! Current cluster fees.
//!
//! The _fees sysvar_ provides access to the [`Fees`] type, which contains the
//! current [`FeeCalculator`].
//!
//! [`Fees`] implements [`Sysvar::get`] and can be loaded efficiently without
//! passing the sysvar account ID to the program.
//!
//! This sysvar is deprecated and will not be available in the future.
//! Transaction fees should be determined with the [`getFeeForMessage`] RPC
//! method. For additional context see the [Comprehensive Compute Fees
//! proposal][ccf].
//!
//! [`getFeeForMessage`]: https://solana.com/docs/rpc/http/getfeeformessage
//! [ccf]: https://docs.solanalabs.com/proposals/comprehensive-compute-fees
//!
//! See also the Solana [documentation on the fees sysvar][sdoc].
//!
//! [sdoc]: https://docs.solanalabs.com/runtime/sysvars#fees

#![allow(deprecated)]

use crate::fee_calculator::FeeCalculator;
#[cfg(feature = "bincode")]
use crate::{impl_sysvar_get, program_error::ProgramError, sysvar::Sysvar};

crate::declare_deprecated_sysvar_id!("SysvarFees111111111111111111111111111111111", Fees);

/// Transaction fees.
#[deprecated(
    since = "1.9.0",
    note = "Please do not use, will no longer be available in the future"
)]
#[repr(C)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fees {
    pub fee_calculator: FeeCalculator,
}

// Recursive expansion of CloneZeroed macro
// =========================================

impl Clone for Fees {
    fn clone(&self) -> Self {
        let mut value = std::mem::MaybeUninit::<Self>::uninit();
        unsafe {
            std::ptr::write_bytes(&mut value, 0, 1);
            let ptr = value.as_mut_ptr();
            std::ptr::addr_of_mut!((*ptr).fee_calculator).write(self.fee_calculator);
            value.assume_init()
        }
    }
}

impl Fees {
    pub fn new(fee_calculator: &FeeCalculator) -> Self {
        #[allow(deprecated)]
        Self {
            fee_calculator: *fee_calculator,
        }
    }
}

#[cfg(feature = "bincode")]
impl Sysvar for Fees {
    impl_sysvar_get!(sol_get_fees_sysvar);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clone() {
        let fees = Fees {
            fee_calculator: FeeCalculator {
                lamports_per_signature: 1,
            },
        };
        let cloned_fees = fees.clone();
        assert_eq!(cloned_fees, fees);
    }
}
