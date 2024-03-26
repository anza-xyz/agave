//! A type to hold data for the [`StakeHistory` sysvar][sv].
//!
//! [sv]: https://docs.solanalabs.com/runtime/sysvars#stakehistory
//!
//! The sysvar ID is declared in [`sysvar::stake_history`].
//!
//! [`sysvar::stake_history`]: crate::sysvar::stake_history

pub use crate::clock::Epoch;
use std::{ops::Deref, sync::Arc};

pub const MAX_ENTRIES: usize = 512; // it should never take as many as 512 epochs to warm up or cool down

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Default, Clone, AbiExample)]
pub struct StakeHistoryEntry {
    pub effective: u64,    // effective stake at this epoch
    pub activating: u64,   // sum of portion of stakes not fully warmed up
    pub deactivating: u64, // requested to be cooled down, not fully deactivated yet
}

impl StakeHistoryEntry {
    pub fn with_effective(effective: u64) -> Self {
        Self {
            effective,
            ..Self::default()
        }
    }

    pub fn with_effective_and_activating(effective: u64, activating: u64) -> Self {
        Self {
            effective,
            activating,
            ..Self::default()
        }
    }

    pub fn with_deactivating(deactivating: u64) -> Self {
        Self {
            effective: deactivating,
            deactivating,
            ..Self::default()
        }
    }
}

impl std::ops::Add for StakeHistoryEntry {
    type Output = StakeHistoryEntry;
    fn add(self, rhs: StakeHistoryEntry) -> Self::Output {
        Self {
            effective: self.effective.saturating_add(rhs.effective),
            activating: self.activating.saturating_add(rhs.activating),
            deactivating: self.deactivating.saturating_add(rhs.deactivating),
        }
    }
}

#[repr(C)]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Default, Clone, AbiExample)]
pub struct StakeHistory(Vec<(Epoch, StakeHistoryEntry)>);

impl StakeHistory {
    // deprecated due to naming clash with `Sysvar::get()`
    #[deprecated(note = "Please use `get_entry` instead")]
    pub fn get(&self, epoch: Epoch) -> Option<&StakeHistoryEntry> {
        self.binary_search_by(|probe| epoch.cmp(&probe.0))
            .ok()
            .map(|index| &self[index].1)
    }

    pub fn add(&mut self, epoch: Epoch, entry: StakeHistoryEntry) {
        match self.binary_search_by(|probe| epoch.cmp(&probe.0)) {
            Ok(index) => (self.0)[index] = (epoch, entry),
            Err(index) => (self.0).insert(index, (epoch, entry)),
        }
        (self.0).truncate(MAX_ENTRIES);
    }
}

impl Deref for StakeHistory {
    type Target = Vec<(Epoch, StakeHistoryEntry)>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub struct StakeHistorySyscall(());

pub trait StakeHistoryGetEntry {
    fn get_entry(&self, epoch: Epoch) -> Option<StakeHistoryEntry>;
}

impl StakeHistoryGetEntry for StakeHistory {
    fn get_entry(&self, epoch: Epoch) -> Option<StakeHistoryEntry> {
        self.binary_search_by(|probe| epoch.cmp(&probe.0))
            .ok()
            .map(|index| self[index].1.clone())
    }
}

impl StakeHistoryGetEntry for StakeHistorySyscall {
    fn get_entry(&self, target_epoch: Epoch) -> Option<StakeHistoryEntry> {
        #[cfg(target_os = "solana")]
        {
            let mut len_buf = [0; 4];
            let len_buf_addr = &mut len_buf as *mut _ as *mut u8;

            let result = unsafe { crate::syscalls::sol_get_sysvar(0, 4, 0, len_buf_addr) };
            if result != crate::entrypoint::SUCCESS {
                panic!("no len???");
            }

            let stake_history_length = u32::from_le_bytes(len_buf);
            crate::msg!("HANA stake history length: {}", stake_history_length);

            if stake_history_length == 0 {
                return None;
            }

            let mut entry_buf = [0; 32];
            let entry_buf_addr = &mut entry_buf as *mut _ as *mut u8;

            let result = unsafe { crate::syscalls::sol_get_sysvar(0, 32, 4, entry_buf_addr) };
            if result != crate::entrypoint::SUCCESS {
                panic!("no entry?????");
            }

            let entry_epoch = bincode::deserialize::<Epoch>(&entry_buf).unwrap();
            if entry_epoch == target_epoch {
                let entry = bincode::deserialize::<StakeHistoryEntry>(&entry_buf[4..]).unwrap();
                crate::msg!("HANA happy happy {} {:?}", entry_epoch, entry);
                return Some(entry);
            } else if entry_epoch > target_epoch {
                // XXX if latest is 15 and target is 12 then delta is 3
                // that means we want stake_history[3] because vec is newest first
                // so delta must be lt length. length is vector length, not byte length
                let epoch_delta = entry_epoch - target_epoch;

                if epoch_delta >= stake_history_length as u64 {
                    panic!("will read oob");
                }

                let offset = epoch_delta * 32 + 4;

                entry_buf.iter_mut().for_each(|x| *x = 0);
                let result =
                    unsafe { crate::syscalls::sol_get_sysvar(0, 32, offset, entry_buf_addr) };
                if result != crate::entrypoint::SUCCESS {
                    panic!("no entry?????");
                }

                let entry_epoch = bincode::deserialize::<Epoch>(&entry_buf).unwrap();
                if entry_epoch == target_epoch {
                    let entry = bincode::deserialize::<StakeHistoryEntry>(&entry_buf[4..]).unwrap();
                    crate::msg!("HANA moderately happy {} {:?}", entry_epoch, entry);
                    return Some(entry);
                } else {
                    panic!(
                        "something terrible has happened: target {}, got {}, delta: {}, offset: {}",
                        target_epoch, entry_epoch, epoch_delta, offset
                    );
                }
            } else {
                panic!(
                    "target epoch {} is newer that latest {}",
                    target_epoch, entry_epoch
                );
            }
        }

        #[cfg(not(target_os = "solana"))]
        {
            panic!("we are NOT bpf");
        }
    }
}

// required for SysvarCache
impl StakeHistoryGetEntry for Arc<StakeHistory> {
    fn get_entry(&self, epoch: Epoch) -> Option<StakeHistoryEntry> {
        self.deref().get_entry(epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stake_history() {
        let mut stake_history = StakeHistory::default();
        for i in 0..MAX_ENTRIES as u64 + 1 {
            stake_history.add(
                i,
                StakeHistoryEntry {
                    activating: i,
                    ..StakeHistoryEntry::default()
                },
            );
        }

        assert_eq!(stake_history.len(), MAX_ENTRIES);
        assert_eq!(stake_history.iter().map(|entry| entry.0).min().unwrap(), 1);

        assert_eq!(stake_history.get_entry(0), None);
        for i in 1..MAX_ENTRIES as u64 + 1 {
            let expected = Some(StakeHistoryEntry {
                activating: i,
                ..StakeHistoryEntry::default()
            });

            assert_eq!(stake_history.get_entry(i), expected);
        }
    }
}
