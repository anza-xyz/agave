use crate::{MAX_ACCOUNTS_PER_TRANSACTION, MAX_INSTRUCTION_TRACE_LENGTH};

pub const SHIFT: u32 = 32;
pub const GUEST_REGION_SIZE: u64 = 1 << SHIFT;

pub const TRANSACTION_FRAME_ADDRESS: u64 = from_index(4);
pub const ACCOUNT_METADATA_AREA: u64 = from_index(5);
pub const INSTRUCTION_TRACE_AREA: u64 = from_index(6);
pub const RETURN_DATA_SCRATCHPAD: u64 = from_index(7);

pub const GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS: u64 = from_index(8);
pub const GUEST_ACCOUNT_PAYLOAD_END_ADDRESS: u64 =
    GUEST_ACCOUNT_PAYLOAD_BASE_ADDRESS + from_index(MAX_ACCOUNTS_PER_TRANSACTION as u64);

const NUM_SYSVARS: u64 = 7;
pub const GUEST_SYSVARS_BASE_ADDRESS: u64 =
    nonoverlapping_base_address(0x1000 - NUM_SYSVARS, GUEST_ACCOUNT_PAYLOAD_END_ADDRESS);
pub const GUEST_SYSVARS_END_ADDRESS: u64 = GUEST_SYSVARS_BASE_ADDRESS + from_index(NUM_SYSVARS);

pub const GUEST_INSTRUCTION_DATA_BASE_ADDRESS: u64 =
    nonoverlapping_base_address(0x1000, GUEST_SYSVARS_END_ADDRESS);
pub const GUEST_INSTRUCTION_DATA_END_ADDRESS: u64 =
    GUEST_INSTRUCTION_DATA_BASE_ADDRESS + from_index(MAX_INSTRUCTION_TRACE_LENGTH as u64);

pub const GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS: u64 =
    nonoverlapping_base_address(0x1040, GUEST_INSTRUCTION_DATA_END_ADDRESS);
pub const GUEST_INSTRUCTION_ACCOUNT_END_ADDRESS: u64 =
    GUEST_INSTRUCTION_ACCOUNT_BASE_ADDRESS + from_index(MAX_INSTRUCTION_TRACE_LENGTH as u64);

pub const fn from_index(index: u64) -> u64 {
    index.checked_mul(GUEST_REGION_SIZE).unwrap()
}

pub const fn nonoverlapping_base_address(base_index: u64, previous_address: u64) -> u64 {
    assert!(from_index(base_index) >= previous_address);
    from_index(base_index)
}

pub const fn abiv2_region_index_from_vm_address(vm_base_address: u64) -> usize {
    (vm_base_address >> SHIFT) as usize
}
