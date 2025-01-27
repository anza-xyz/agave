#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
//! An upgradeable BPF loader native program.
//!
//! The upgradeable BPF loader is responsible for deploying, upgrading, and
//! executing BPF programs. The upgradeable loader allows a program's authority
//! to update the program at any time. This ability breaks the "code is law"
//! contract that once a program is on-chain it is immutable. Because of this,
//! care should be taken before executing upgradeable programs which still have
//! a functioning authority. For more information refer to the
//! [`instruction`] module.
//!
//! The `solana program deploy` CLI command uses the
//! upgradeable BPF loader. Calling `solana program deploy --final` deploys a
//! program that cannot be upgraded, but it does so by revoking the authority to
//! upgrade, not by using the non-upgradeable loader.
//!
//! [`instruction`]: crate::instruction

use solana_pubkey::Pubkey;

pub mod instruction;
pub mod state;

/// Returns the program data address for a program ID
pub fn get_program_data_address(program_address: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[program_address.as_ref()],
        &solana_sdk_ids::bpf_loader_upgradeable::id(),
    )
    .0
}
