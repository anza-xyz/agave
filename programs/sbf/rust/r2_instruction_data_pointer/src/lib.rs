//! Test program that reads instruction data using the r2 register pointer.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::missing_safety_doc)]

#[unsafe(no_mangle)]
pub unsafe extern "C" fn entrypoint(_input: *mut u8, instruction_data_addr: *const u8) -> u64 {
    let instruction_data_len = unsafe { *((instruction_data_addr as u64 - 8) as *const u64) };
    let instruction_data = unsafe {
        core::slice::from_raw_parts(instruction_data_addr, instruction_data_len as usize)
    };

    solana_cpi::set_return_data(instruction_data);

    solana_program_entrypoint::SUCCESS
}

solana_program_entrypoint::custom_heap_default!();
solana_program_entrypoint::custom_panic_default!();
