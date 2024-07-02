//! The most recent hashes of a slot's parent banks.
//!
//! The _slot hashes sysvar_ provides access to the [`SlotHashes`] type.
//!
//! The [`Sysvar::from_account_info`] and [`Sysvar::get`] methods always return
//! [`ProgramError::UnsupportedSysvar`] because this sysvar account is too large
//! to process on-chain. Thus this sysvar cannot be accessed on chain, though
//! one can still use the [`SysvarId::id`], [`SysvarId::check_id`] and
//! [`Sysvar::size_of`] methods in an on-chain program, and it can be accessed
//! off-chain through RPC.
//!
//! [`SysvarId::id`]: crate::sysvar::SysvarId::id
//! [`SysvarId::check_id`]: crate::sysvar::SysvarId::check_id
//!
//! # Examples
//!
//! Calling via the RPC client:
//!
//! ```
//! # use solana_program::example_mocks::solana_sdk;
//! # use solana_program::example_mocks::solana_rpc_client;
//! # use solana_sdk::account::Account;
//! # use solana_rpc_client::rpc_client::RpcClient;
//! # use solana_sdk::sysvar::slot_hashes::{self, SlotHashes};
//! # use anyhow::Result;
//! #
//! fn print_sysvar_slot_hashes(client: &RpcClient) -> Result<()> {
//! #   client.set_get_account_response(slot_hashes::ID, Account {
//! #       lamports: 1009200,
//! #       data: vec![1, 0, 0, 0, 0, 0, 0, 0, 86, 190, 235, 7, 0, 0, 0, 0, 133, 242, 94, 158, 223, 253, 207, 184, 227, 194, 235, 27, 176, 98, 73, 3, 175, 201, 224, 111, 21, 65, 73, 27, 137, 73, 229, 19, 255, 192, 193, 126],
//! #       owner: solana_sdk::system_program::ID,
//! #       executable: false,
//! #       rent_epoch: 307,
//! # });
//! #
//!     let slot_hashes = client.get_account(&slot_hashes::ID)?;
//!     let data: SlotHashes = bincode::deserialize(&slot_hashes.data)?;
//!
//!     Ok(())
//! }
//! #
//! # let client = RpcClient::new(String::new());
//! # print_sysvar_slot_hashes(&client)?;
//! #
//! # Ok::<(), anyhow::Error>(())
//! ```

pub use crate::slot_hashes::SlotHashes;
use {
    crate::{
        account_info::AccountInfo,
        clock::Slot,
        hash::Hash,
        program_error::ProgramError,
        slot_hashes::SlotHash,
        sysvar::{get_sysvar, Sysvar, SysvarId},
    },
    bytemuck_derive::{Pod, Zeroable},
};

crate::declare_sysvar_id!("SysvarS1otHashes111111111111111111111111111", SlotHashes);

impl Sysvar for SlotHashes {
    // override
    fn size_of() -> usize {
        // hard-coded so that we don't have to construct an empty
        20_488 // golden, update if MAX_ENTRIES changes
    }
    fn from_account_info(_account_info: &AccountInfo) -> Result<Self, ProgramError> {
        // This sysvar is too large to bincode::deserialize in-program
        Err(ProgramError::UnsupportedSysvar)
    }
}

const VEC_LEN_U64_SIZE: usize = std::mem::size_of::<u64>();

/// A bytemuck-compatible representation of a `SlotHash`.
#[derive(Copy, Clone, Default, Pod, Zeroable)]
#[repr(C)]
pub struct PodSlotHash {
    slot: Slot,
    hash: Hash,
}

/// API for querying the `SlotHashes` sysvar.
pub struct SlotHashesSysvar;

impl SlotHashesSysvar {
    /// Get a value from the sysvar entries by its key.
    /// Returns `None` if the key is not found.
    pub fn get(slot: &Slot) -> Result<Option<Hash>, ProgramError> {
        Self::get_pod_slot_hashes().map(|pod_hashes| {
            pod_hashes
                .binary_search_by(|PodSlotHash { slot: this, .. }| slot.cmp(this))
                .map(|idx| pod_hashes[idx].hash)
                .ok()
        })
    }

    /// Get the position of an entry in the sysvar by its key.
    /// Returns `None` if the key is not found.
    pub fn position(slot: &Slot) -> Result<Option<usize>, ProgramError> {
        Self::get_pod_slot_hashes().map(|pod_hashes| {
            pod_hashes
                .binary_search_by(|PodSlotHash { slot: this, .. }| slot.cmp(this))
                .ok()
        })
    }

    /// Return the slot hashes sysvar as a vector of `PodSlotHash`.
    pub fn get_pod_slot_hashes() -> Result<Vec<PodSlotHash>, ProgramError> {
        // First fetch the length of the slot hashes vector.
        let slot_hash_count = {
            let mut data = vec![0u8; VEC_LEN_U64_SIZE];
            get_sysvar(
                &mut data,
                &SlotHashes::id(),
                /* offset */ 0,
                VEC_LEN_U64_SIZE as u64,
            )?;
            usize::from_le_bytes(data.try_into().unwrap())
        };

        if slot_hash_count == 0 {
            return Ok(vec![]);
        }

        // Then fetch the sysvar data.
        let length = slot_hash_count
            .checked_mul(std::mem::size_of::<SlotHash>())
            .ok_or(ProgramError::ArithmeticOverflow)?;

        let mut data = vec![0u8; length];
        get_sysvar(
            &mut data,
            &SlotHashes::id(),
            VEC_LEN_U64_SIZE as u64,
            length as u64,
        )?;

        // Finally, convert to a vector of `PodSlotHash`.
        bytemuck::try_cast_slice::<u8, PodSlotHash>(&data)
            .ok()
            .map(|pod_hashes| pod_hashes.to_vec())
            .ok_or(ProgramError::InvalidAccountData)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            clock::Slot,
            hash::{hash, Hash},
            slot_hashes::MAX_ENTRIES,
            sysvar::tests::mock_get_sysvar_syscall,
        },
        serial_test::serial,
        test_case::test_case,
    };

    #[test]
    fn test_size_of() {
        assert_eq!(
            SlotHashes::size_of(),
            bincode::serialized_size(
                &(0..MAX_ENTRIES)
                    .map(|slot| (slot as Slot, Hash::default()))
                    .collect::<SlotHashes>()
            )
            .unwrap() as usize
        );
    }

    #[allow(clippy::arithmetic_side_effects)]
    #[test_case(0)]
    #[test_case(1)]
    #[test_case(2)]
    #[test_case(5)]
    #[test_case(10)]
    #[test_case(64)]
    #[test_case(128)]
    #[test_case(192)]
    #[test_case(256)]
    #[test_case(384)]
    #[test_case(MAX_ENTRIES)]
    #[serial]
    fn test_slot_hashes_sysvar(num_entries: usize) {
        let mut slot_hashes = vec![];
        for i in 0..num_entries {
            slot_hashes.push((
                i as u64,
                hash(&[(i >> 24) as u8, (i >> 16) as u8, (i >> 8) as u8, i as u8]),
            ));
        }

        let check_slot_hashes = SlotHashes::new(&slot_hashes);
        mock_get_sysvar_syscall(&bincode::serialize(&check_slot_hashes).unwrap());

        // `get_pod_slot_hashes` should match the slot hashes.
        // Note slot hashes are stored largest slot to smallest.
        for (i, pod_slot_hash) in SlotHashesSysvar::get_pod_slot_hashes()
            .unwrap()
            .iter()
            .enumerate()
        {
            let check = slot_hashes[num_entries - 1 - i];
            assert_eq!(pod_slot_hash.slot, check.0);
            assert_eq!(pod_slot_hash.hash, check.1);
        }

        // Check some arbitrary slots in the created slot hashes.
        let num_entries = num_entries as Slot;
        let check_slots = if num_entries == 0 {
            vec![num_entries, num_entries + 100]
        } else {
            vec![
                0,
                num_entries / 4,
                num_entries / 2,
                num_entries - 1,
                num_entries,
                num_entries + 100,
            ]
        };

        for slot in check_slots.iter() {
            // `get`:
            assert_eq!(
                SlotHashesSysvar::get(slot).unwrap().as_ref(),
                check_slot_hashes.get(slot),
            );
            // `position`:
            assert_eq!(
                SlotHashesSysvar::position(slot).unwrap(),
                check_slot_hashes.position(slot),
            );
        }
    }
}
