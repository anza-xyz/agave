#[allow(deprecated)]
use solana_sdk::sysvar::{
    fees::Fees, last_restart_slot::LastRestartSlot, recent_blockhashes::RecentBlockhashes,
};
use {
    crate::invoke_context::InvokeContext,
    serde::de::DeserializeOwned,
    solana_sdk::{
        instruction::InstructionError,
        pubkey::Pubkey,
        sysvar::{
            clock::Clock, epoch_rewards::EpochRewards, epoch_schedule::EpochSchedule, rent::Rent,
            slot_hashes::SlotHashes, stake_history::StakeHistory, Sysvar, SysvarId,
        },
        transaction_context::{IndexOfAccount, InstructionContext, TransactionContext},
    },
    std::sync::Arc,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CachedSysvar {
    Clock,
    EpochSchedule,
    EpochRewards,
    Rent,
    SlotHashes,
    StakeHistory,
    LastRestartSlot,
}

#[derive(Default, Clone, Debug)]
pub struct SysvarCache {
    clock: Option<Vec<u8>>,
    epoch_schedule: Option<Vec<u8>>,
    epoch_rewards: Option<Vec<u8>>,
    rent: Option<Vec<u8>>,
    slot_hashes: Option<Vec<u8>>,
    stake_history: Option<Vec<u8>>,
    last_restart_slot: Option<Vec<u8>>,
}

impl SysvarCache {
    fn vec_for_enum(&self, sysvar_type: CachedSysvar) -> &Option<Vec<u8>> {
        match sysvar_type {
            CachedSysvar::Clock => &self.clock,
            CachedSysvar::EpochSchedule => &self.epoch_schedule,
            CachedSysvar::EpochRewards => &self.epoch_rewards,
            CachedSysvar::Rent => &self.rent,
            CachedSysvar::SlotHashes => &self.slot_hashes,
            CachedSysvar::StakeHistory => &self.stake_history,
            CachedSysvar::LastRestartSlot => &self.last_restart_slot,
        }
    }

    pub fn read_sysvar_into(
        &self,
        sysvar_type: CachedSysvar,
        length: usize,
        offset: usize,
        out_buf: &mut [u8],
    ) -> Result<(), InstructionError> {
        if let Some(ref sysvar_buf) = self.vec_for_enum(sysvar_type) {
            if length == 0 {
                panic!("zero length error");
            }

            if length + offset > sysvar_buf.len() {
                panic!("overrun error");
            }

            if length != out_buf.len() {
                panic!("bad out_buf error");
            }

            out_buf.copy_from_slice(&sysvar_buf[offset..offset + length]);

            Ok(())
        } else {
            panic!("not found err");
        }
    }

    fn get_sysvar_obj<T: DeserializeOwned>(
        &self,
        sysvar_type: CachedSysvar,
    ) -> Result<T, InstructionError> {
        if let Some(ref sysvar_buf) = self.vec_for_enum(sysvar_type) {
            bincode::deserialize(sysvar_buf).map_err(|_| InstructionError::UnsupportedSysvar)
        } else {
            Err(InstructionError::UnsupportedSysvar)
        }
    }

    pub fn get_clock(&self) -> Result<Clock, InstructionError> {
        self.get_sysvar_obj(CachedSysvar::Clock)
    }

    pub fn get_epoch_schedule(&self) -> Result<EpochSchedule, InstructionError> {
        self.get_sysvar_obj(CachedSysvar::EpochSchedule)
    }

    pub fn get_epoch_rewards(&self) -> Result<EpochRewards, InstructionError> {
        self.get_sysvar_obj(CachedSysvar::EpochRewards)
    }

    pub fn get_rent(&self) -> Result<Rent, InstructionError> {
        self.get_sysvar_obj(CachedSysvar::Rent)
    }

    pub fn get_last_restart_slot(&self) -> Result<LastRestartSlot, InstructionError> {
        self.get_sysvar_obj(CachedSysvar::LastRestartSlot)
    }

    // XXX remove after fixing callsites
    pub fn get_stake_history(&self) -> Result<StakeHistory, InstructionError> {
        self.get_sysvar_obj(CachedSysvar::StakeHistory)
    }

    // XXX remove after fixing callsites
    pub fn get_slot_hashes(&self) -> Result<SlotHashes, InstructionError> {
        self.get_sysvar_obj(CachedSysvar::SlotHashes)
    }

    // XXX investigate if we can actually nix this
    pub fn get_fees(&self) -> Result<Fees, InstructionError> {
        Err(InstructionError::UnsupportedSysvar)
    }

    // XXX investigate if we can actually nix this
    pub fn get_recent_blockhashes(&self) -> Result<RecentBlockhashes, InstructionError> {
        Err(InstructionError::UnsupportedSysvar)
    }

    pub fn fill_missing_entries<F: FnMut(&Pubkey, &mut dyn FnMut(&[u8]))>(
        &mut self,
        mut get_account_data: F,
    ) {
        if self.clock.is_none() {
            get_account_data(&Clock::id(), &mut |data: &[u8]| {
                if let Ok(clock) = bincode::deserialize(data) {
                    self.clock = Some(clock);
                }
            });
        }

        if self.epoch_schedule.is_none() {
            get_account_data(&EpochSchedule::id(), &mut |data: &[u8]| {
                if let Ok(epoch_schedule) = bincode::deserialize(data) {
                    self.epoch_schedule = Some(epoch_schedule);
                }
            });
        }

        if self.epoch_rewards.is_none() {
            get_account_data(&EpochRewards::id(), &mut |data: &[u8]| {
                if let Ok(epoch_rewards) = bincode::deserialize(data) {
                    self.epoch_rewards = Some(epoch_rewards);
                }
            });
        }

        if self.rent.is_none() {
            get_account_data(&Rent::id(), &mut |data: &[u8]| {
                if let Ok(rent) = bincode::deserialize(data) {
                    self.rent = Some(rent);
                }
            });
        }

        if self.slot_hashes.is_none() {
            get_account_data(&SlotHashes::id(), &mut |data: &[u8]| {
                if let Ok(slot_hashes) = bincode::deserialize(data) {
                    self.slot_hashes = Some(slot_hashes);
                }
            });
        }

        if self.stake_history.is_none() {
            get_account_data(&StakeHistory::id(), &mut |data: &[u8]| {
                if let Ok(stake_history) = bincode::deserialize(data) {
                    self.stake_history = Some(stake_history);
                }
            });
        }

        if self.last_restart_slot.is_none() {
            get_account_data(&LastRestartSlot::id(), &mut |data: &[u8]| {
                if let Ok(last_restart_slot) = bincode::deserialize(data) {
                    self.last_restart_slot = Some(last_restart_slot);
                }
            });
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// These methods facilitate a transition from fetching sysvars from keyed
/// accounts to fetching from the sysvar cache without breaking consensus. In
/// order to keep consistent behavior, they continue to enforce the same checks
/// as `solana_sdk::keyed_account::from_keyed_account` despite dynamically
/// loading them instead of deserializing from account data.
pub mod get_sysvar_with_account_check {
    use super::*;

    fn check_sysvar_account<S: Sysvar>(
        transaction_context: &TransactionContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<(), InstructionError> {
        let index_in_transaction = instruction_context
            .get_index_of_instruction_account_in_transaction(instruction_account_index)?;
        if !S::check_id(transaction_context.get_key_of_account_at_index(index_in_transaction)?) {
            return Err(InstructionError::InvalidArgument);
        }
        Ok(())
    }

    pub fn clock(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Clock, InstructionError> {
        check_sysvar_account::<Clock>(
            invoke_context.transaction_context,
            instruction_context,
            instruction_account_index,
        )?;
        invoke_context.get_sysvar_cache().get_clock()
    }

    pub fn rent(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Rent, InstructionError> {
        check_sysvar_account::<Rent>(
            invoke_context.transaction_context,
            instruction_context,
            instruction_account_index,
        )?;
        invoke_context.get_sysvar_cache().get_rent()
    }

    pub fn slot_hashes(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<SlotHashes, InstructionError> {
        check_sysvar_account::<SlotHashes>(
            invoke_context.transaction_context,
            instruction_context,
            instruction_account_index,
        )?;
        invoke_context.get_sysvar_cache().get_slot_hashes()
    }

    #[allow(deprecated)]
    pub fn recent_blockhashes(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<RecentBlockhashes, InstructionError> {
        check_sysvar_account::<RecentBlockhashes>(
            invoke_context.transaction_context,
            instruction_context,
            instruction_account_index,
        )?;
        invoke_context.get_sysvar_cache().get_recent_blockhashes()
    }

    pub fn stake_history(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<StakeHistory, InstructionError> {
        check_sysvar_account::<StakeHistory>(
            invoke_context.transaction_context,
            instruction_context,
            instruction_account_index,
        )?;
        invoke_context.get_sysvar_cache().get_stake_history()
    }

    pub fn last_restart_slot(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<LastRestartSlot, InstructionError> {
        check_sysvar_account::<LastRestartSlot>(
            invoke_context.transaction_context,
            instruction_context,
            instruction_account_index,
        )?;
        invoke_context.get_sysvar_cache().get_last_restart_slot()
    }
}
