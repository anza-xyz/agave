//! Test program that reads instruction data using the r2 register pointer.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::missing_safety_doc)]

#[no_mangle]
pub unsafe extern "C" fn entrypoint(_input: *mut u8, instruction_data_addr: u64) -> u64 {
    let instruction_data_len = *((instruction_data_addr - 8) as *const u64);
    let instruction_data = core::slice::from_raw_parts(
        instruction_data_addr as *const u8,
        instruction_data_len as usize,
    );

    solana_cpi::set_return_data(instruction_data);

    solana_program_entrypoint::SUCCESS
}

solana_program_entrypoint::custom_heap_default!();
solana_program_entrypoint::custom_panic_default!();
