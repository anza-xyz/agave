//! Access to special accounts with dynamically-updated data.
//!
//! Sysvars are special accounts that contain dynamically-updated data about the
//! network cluster, the blockchain history, and the executing transaction. Each
//! sysvar is defined in its own crate. The [`clock`], [`epoch_schedule`],
//! [`instructions`], and [`rent`] sysvars are most useful to on-chain programs.
//!
//! [`clock`]: https://docs.rs/solana-clock/latest
//! [`epoch_schedule`]: https://docs.rs/solana-epoch-schedule/latest
//! [`instructions`]: https://docs.rs/solana-program/latest/solana_program/sysvar/instructions
//! [`rent`]: https://docs.rs/solana-rent/latest
//!
//! All sysvar accounts are owned by the account identified by [`solana_sysvar::ID`].
//!
//! [`solana_sysvar::ID`]: crate::ID
//!
//! For more details see the Solana [documentation on sysvars][sysvardoc].
//!
//! [sysvardoc]: https://docs.solanalabs.com/runtime/sysvars

#[cfg(target_os = "solana")]
pub mod syscalls;

use solana_program_error::ProgramError;
/// Re-export types required for macros
pub use solana_pubkey::{declare_deprecated_id, declare_id, Pubkey};

/// A type that holds sysvar data and has an associated sysvar `Pubkey`.
pub trait SysvarId {
    /// The `Pubkey` of the sysvar.
    fn id() -> Pubkey;

    /// Returns `true` if the given pubkey is the program ID.
    fn check_id(pubkey: &Pubkey) -> bool;
}

/// Declares an ID that implements [`SysvarId`].
#[macro_export]
macro_rules! declare_sysvar_id(
    ($name:expr, $type:ty) => (
        $crate::declare_id!($name);

        impl $crate::SysvarId for $type {
            fn id() -> $crate::Pubkey {
                id()
            }

            fn check_id(pubkey: &$crate::Pubkey) -> bool {
                check_id(pubkey)
            }
        }
    )
);

/// Same as [`declare_sysvar_id`] except that it reports that this ID has been deprecated.
#[macro_export]
macro_rules! declare_deprecated_sysvar_id(
    ($name:expr, $type:ty) => (
        $crate::declare_deprecated_id!($name);

        impl $crate::SysvarId for $type {
            fn id() -> $crate::Pubkey {
                #[allow(deprecated)]
                id()
            }

            fn check_id(pubkey: &$crate::Pubkey) -> bool {
                #[allow(deprecated)]
                check_id(pubkey)
            }
        }
    )
);

const _SUCCESS: u64 = 0;
#[cfg(test)]
static_assertions::const_assert_eq!(_SUCCESS, solana_program_entrypoint::SUCCESS);

// Owner pubkey for sysvar accounts
solana_pubkey::declare_id!("Sysvar1111111111111111111111111111111111111");

/// Handler for retrieving a slice of sysvar data from the `sol_get_sysvar`
/// syscall.
pub fn get_sysvar(
    dst: &mut [u8],
    sysvar_id: &Pubkey,
    offset: u64,
    length: u64,
) -> Result<(), ProgramError> {
    // Check that the provided destination buffer is large enough to hold the
    // requested data.
    if dst.len() < length as usize {
        return Err(ProgramError::InvalidArgument);
    }

    get_sysvar_unchecked(dst, sysvar_id, offset, length)
}

/// Handler for retrieving a slice of sysvar data from the `sol_get_sysvar`
/// syscall, without a client-side length check
#[allow(unused_variables)]
pub fn get_sysvar_unchecked(
    dst: &mut [u8],
    sysvar_id: &Pubkey,
    offset: u64,
    length: u64,
) -> Result<(), ProgramError> {
    #[cfg(target_os = "solana")]
    {
        let sysvar_id = sysvar_id as *const _ as *const u8;
        let var_addr = dst as *mut _ as *mut u8;

        let result =
            unsafe { crate::syscalls::sol_get_sysvar(sysvar_id, var_addr, offset, length) };

        match result {
            _SUCCESS => Ok(()),
            e => Err(e.into()),
        }
    }
    #[cfg(not(target_os = "solana"))]
    Err(ProgramError::UnsupportedSysvar)
}
