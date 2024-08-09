//! Information about the last restart slot (hard fork).

use crate::clock::Slot;

#[repr(C)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
pub struct LastRestartSlot {
    /// The last restart `Slot`.
    pub last_restart_slot: Slot,
}

// Recursive expansion of CloneZeroed macro
// =========================================
impl Clone for LastRestartSlot {
    fn clone(&self) -> Self {
        let mut value = std::mem::MaybeUninit::<Self>::uninit();
        unsafe {
            std::ptr::write_bytes(&mut value, 0, 1);
            let ptr = value.as_mut_ptr();
            std::ptr::addr_of_mut!((*ptr).last_restart_slot).write(self.last_restart_slot);
            value.assume_init()
        }
    }
}
