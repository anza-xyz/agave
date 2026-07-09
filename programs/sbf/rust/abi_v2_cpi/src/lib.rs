#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::missing_safety_doc)]

use {
    core::slice,
    solana_transaction_context::{
        instruction::InstructionFrame,
        instruction_accounts::InstructionAccount,
        transaction::TransactionFrame,
        transaction_accounts::AccountSharedFields,
        vm_addresses::{
            ACCOUNT_METADATA_AREA, GUEST_REGION_SIZE, INSTRUCTION_TRACE_AREA,
            TRANSACTION_FRAME_ADDRESS,
        },
    },
    std::{alloc::Layout, ptr::null_mut},
};

#[global_allocator]
static A: BumpAllocator =
    unsafe { BumpAllocator::with_fixed_address_range(0x300000000, 32 * 1024) };

pub struct BumpAllocator {
    start: usize,
    len: usize,
}

impl BumpAllocator {
    #[inline]
    #[allow(clippy::arithmetic_side_effects)]
    pub unsafe fn new(arena: &mut [u8]) -> Self {
        debug_assert!(
            arena.len() > size_of::<usize>(),
            "Arena should be larger than usize"
        );

        // create a pointer to the start of the arena
        // that will hold an address of the byte following free space
        let pos_ptr = arena.as_mut_ptr() as *mut usize;
        // initialize the data there
        *pos_ptr = pos_ptr as usize + arena.len();

        Self {
            start: pos_ptr as usize,
            len: arena.len(),
        }
    }

    pub const unsafe fn with_fixed_address_range(start: usize, len: usize) -> Self {
        Self { start, len }
    }
}

#[allow(clippy::arithmetic_side_effects)]
unsafe impl std::alloc::GlobalAlloc for BumpAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pos_ptr = self.start as *mut usize;
        let mut pos = *pos_ptr;
        if pos == 0 {
            // First time, set starting position
            pos = self.start + self.len;
        }
        pos = pos.saturating_sub(layout.size());
        pos &= !(layout.align().wrapping_sub(1));
        if pos < self.start + size_of::<*mut u8>() {
            return null_mut();
        }
        *pos_ptr = pos;
        pos as *mut u8
    }
    #[inline]
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {
        // I'm a bump allocator, I don't free
    }
}

fn sol_log(message: &[u8]) {
    unsafe {
        let syscall: extern "C" fn(*const u8, u64) = core::mem::transmute(544561597u64); // murmur32 hash of "sol_log_"
        syscall(message.as_ptr(), message.len() as u64)
    }
}

#[unsafe(no_mangle)]
extern "C" fn custom_panic(info: &core::panic::PanicInfo<'_>) {
    let formatted = format!("{info:?}");
    sol_log(formatted.as_bytes());
}

fn set_buffer_length(base_address: u64, new_length: u64) -> u64 {
    unsafe {
        let syscall: extern "C" fn(u64, u64, u64, u64, u64) -> u64 =
            core::mem::transmute(0x713026f5u64);
        syscall(base_address, new_length, 0, 0, 0)
    }
}

fn invoke_cpi(program_idx: u64, signer_seeds_ptr: u64, signer_seeds_len: u64) {
    unsafe {
        let syscall: extern "C" fn(u64, u64, u64) = core::mem::transmute(2722332484u64);
        syscall(program_idx, signer_seeds_ptr, signer_seeds_len);
    }
}

unsafe fn perform_checks_inside_cpi(
    tx_frame: &mut TransactionFrame,
    current_ix: &InstructionFrame,
) {
    let ix_accounts = current_ix.instruction_accounts.as_slice();

    // All accounts are supposed to be readonly
    for account in ix_accounts.iter() {
        assert!(!account.is_writable());
    }

    assert_eq!(current_ix.instruction_data.as_slice(), b"Hello!");

    assert_eq!(current_ix.nesting_level, 1);
    assert_eq!(current_ix.index_of_caller_instruction, 0);

    assert_eq!(
        tx_frame.cpi_data_scratchpad.ptr(),
        current_ix
            .instruction_data
            .ptr()
            .saturating_add(GUEST_REGION_SIZE)
    );
    assert_eq!(
        tx_frame.cpi_accounts_scratchpad.ptr(),
        current_ix
            .instruction_accounts
            .ptr()
            .saturating_add(GUEST_REGION_SIZE)
    );

    // Let's write something to return data
    let return_string = b"Hi!";
    set_buffer_length(
        tx_frame.return_data_scratchpad.ptr(),
        return_string.len() as u64,
    );
    assert_eq!(
        tx_frame.return_data_scratchpad.len(),
        return_string.len() as u64
    );
    let return_data_buffer_mut = tx_frame.return_data_scratchpad.as_slice_mut();
    return_data_buffer_mut.copy_from_slice(return_string);
}

#[unsafe(no_mangle)]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn entrypoint() -> u64 {
    // Transaction frame
    let tx_frame_ptr = TRANSACTION_FRAME_ADDRESS as *mut TransactionFrame;
    let tx_frame = &mut *tx_frame_ptr;

    let instruction_trace = slice::from_raw_parts(
        INSTRUCTION_TRACE_AREA as *const InstructionFrame,
        tx_frame.total_number_of_instructions_in_trace as usize,
    );

    let current_ix = instruction_trace
        .get(tx_frame.current_executing_instruction as usize)
        .unwrap();

    if current_ix.nesting_level > 0 {
        perform_checks_inside_cpi(tx_frame, current_ix);
        return 0;
    }

    // Prepare CPI data
    let cpi_data = b"Hello!";
    set_buffer_length(tx_frame.cpi_data_scratchpad.ptr(), cpi_data.len() as u64);
    let cpi_data_scratchpad_mut = tx_frame.cpi_data_scratchpad.as_slice_mut();
    assert_eq!(cpi_data_scratchpad_mut.len(), cpi_data.len());
    cpi_data_scratchpad_mut.copy_from_slice(cpi_data);

    let ix_accounts = current_ix.instruction_accounts.as_slice();
    // Ensure all accounts are writable (except the first which is the program to be called),
    // so we can see if we restrict visibility in CPI
    for account in ix_accounts.iter().skip(1) {
        assert!(account.is_writable());
    }

    // Prepare CPI accounts
    set_buffer_length(
        tx_frame.cpi_accounts_scratchpad.ptr(),
        size_of::<InstructionAccount>().saturating_mul(2) as u64,
    );
    let cpi_accounts_scratchpad_mut = tx_frame.cpi_accounts_scratchpad.as_slice_mut();
    assert_eq!(cpi_accounts_scratchpad_mut.len(), 2);
    let acc_0 = cpi_accounts_scratchpad_mut.get_unchecked_mut(0);
    *acc_0 = *ix_accounts.get(1).unwrap();
    acc_0.set_is_writable(false);

    let acc_1 = cpi_accounts_scratchpad_mut.get_unchecked_mut(1);
    *acc_1 = *ix_accounts.get(2).unwrap();
    acc_1.set_is_writable(false);

    let callee_program = ix_accounts.get_unchecked(0).index_in_transaction;
    invoke_cpi(callee_program as u64, 0, 0);

    // Checks after CPI
    let tx_accounts_metadata = slice::from_raw_parts(
        ACCOUNT_METADATA_AREA as *mut AccountSharedFields,
        tx_frame.number_of_transaction_accounts as usize,
    );

    assert_eq!(tx_frame.return_data_scratchpad.as_slice(), b"Hi!");
    assert_eq!(
        tx_frame.return_data_pubkey,
        tx_accounts_metadata
            .get_unchecked(callee_program as usize)
            .key
    );

    0
}
