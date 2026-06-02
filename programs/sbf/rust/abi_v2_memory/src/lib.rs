#![allow(unsafe_op_in_unsafe_fn)]

use {
    core::{alloc::Layout, ptr::null_mut, slice},
    solana_pubkey::Pubkey,
    solana_transaction_context::{
        instruction::InstructionFrame,
        transaction::TransactionFrame,
        transaction_accounts::AccountSharedFields,
        vm_addresses::{
            ACCOUNT_METADATA_AREA, GUEST_INSTRUCTION_DATA_BASE_ADDRESS, GUEST_REGION_SIZE,
            INSTRUCTION_TRACE_AREA, RETURN_DATA_SCRATCHPAD, TRANSACTION_FRAME_ADDRESS,
        },
    },
};

fn sol_log(message: &[u8]) {
    unsafe {
        let syscall: extern "C" fn(*const u8, u64) = core::mem::transmute(544561597u64); // murmur32 hash of "sol_log_"
        syscall(message.as_ptr(), message.len() as u64)
    }
}

fn set_buffer_length(base_address: u64, new_length: u64) -> u64 {
    unsafe {
        let syscall: extern "C" fn(u64, u64, u64, u64, u64) -> u64 =
            core::mem::transmute(0x713026f5u64);
        syscall(base_address as u64, new_length, 0, 0, 0)
    }
}

fn assign_owner(account_idx: u64, new_owner: *const Pubkey) {
    unsafe {
        let syscall: extern "C" fn(u64, *const Pubkey) = core::mem::transmute(4042720265u64);
        syscall(account_idx, new_owner);
    }
}

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

#[unsafe(no_mangle)]
extern "C" fn custom_panic(info: &core::panic::PanicInfo<'_>) {
    let formatted = format!("{info:?}");
    sol_log(formatted.as_bytes());
}

unsafe fn test_valid_accesses(
    tx_frame: &TransactionFrame,
    tx_accounts_metadata: &[AccountSharedFields],
) {
    // Transaction frame
    let instruction_trace = slice::from_raw_parts(
        INSTRUCTION_TRACE_AREA as *const InstructionFrame,
        tx_frame.total_number_of_instructions_in_trace as usize,
    );

    let current_ix = instruction_trace
        .get(tx_frame.current_executing_instruction as usize)
        .unwrap();
    let program_id = &tx_accounts_metadata
        .get(current_ix.program_account_index_in_tx as usize)
        .unwrap()
        .key;

    assert_eq!(
        tx_frame.return_data_pubkey.to_bytes(),
        program_id.to_bytes()
    );
    assert_eq!(
        tx_frame.return_data_scratchpad.ptr(),
        RETURN_DATA_SCRATCHPAD
    );
    assert_eq!(tx_frame.return_data_scratchpad.len(), 0);
    assert_eq!(tx_frame.total_number_of_instructions_in_trace, 2);
    assert_eq!(tx_frame.number_of_cpis_in_trace, 0);
    assert_eq!(tx_frame.number_of_transaction_accounts, 7);
    assert_eq!(
        tx_frame.cpi_scratchpad.ptr(),
        GUEST_INSTRUCTION_DATA_BASE_ADDRESS.saturating_add(
            GUEST_REGION_SIZE.saturating_mul(tx_frame.total_number_of_instructions_in_trace as u64)
        )
    );
    assert_eq!(tx_frame.cpi_scratchpad.len(), 0);

    assert_eq!(current_ix.nesting_level, 0);
    assert_eq!(current_ix.index_of_caller_instruction, u16::MAX);

    if tx_frame.current_executing_instruction == 0 {
        let ix_accounts = current_ix.instruction_accounts.deref();
        assert_eq!(ix_accounts.len(), 2);
        assert!(
            ix_accounts.get_unchecked(0).is_writable() && ix_accounts.get_unchecked(0).is_signer()
        );
        assert!(
            !(ix_accounts.get_unchecked(1).is_signer()
                || ix_accounts.get_unchecked(1).is_writable())
        );

        let acc_1 = tx_accounts_metadata
            .get_unchecked(ix_accounts.get_unchecked(0).index_in_transaction as usize);
        assert_eq!(acc_1.lamports, 223450);
        assert_eq!(acc_1.owner.to_bytes(), [0u8; 32]);
        assert_eq!(acc_1.payload.deref(), &[1, 2, 3]);

        let acc_2 = tx_accounts_metadata
            .get_unchecked(ix_accounts.get_unchecked(1).index_in_transaction as usize);
        assert_eq!(acc_2.lamports, 90123);
        assert_eq!(acc_2.key.to_bytes(), acc_2.payload.deref());

        assert_eq!(current_ix.instruction_data.deref(), b"\x02");
    } else if tx_frame.current_executing_instruction == 1 {
        let ix_accounts = current_ix.instruction_accounts.deref();
        assert_eq!(ix_accounts.len(), 2);
        assert!(
            !ix_accounts.get_unchecked(0).is_writable() && ix_accounts.get_unchecked(0).is_signer()
        );
        assert!(
            !ix_accounts.get_unchecked(1).is_signer() && ix_accounts.get_unchecked(1).is_writable()
        );

        let acc_1 = tx_accounts_metadata
            .get_unchecked(ix_accounts.get_unchecked(0).index_in_transaction as usize);
        assert_eq!(acc_1.lamports, 35);
        assert_eq!(acc_1.owner.to_bytes(), [0u8; 32]);
        assert_eq!(acc_1.payload.deref(), &[3, 4, 5]);

        let acc_2 = tx_accounts_metadata
            .get_unchecked(ix_accounts.get_unchecked(1).index_in_transaction as usize);
        assert_eq!(acc_2.lamports, 9123);
        assert_eq!(acc_2.owner, acc_1.key);

        assert_eq!(current_ix.instruction_data.deref(), b"\x03");
    } else {
        panic!("Not expecting more than two instructions.")
    }
}

unsafe fn read_invalid_regions(current_ix: &InstructionFrame) -> u64 {
    let ptr_to_val = current_ix.instruction_data.ptr().saturating_add(1);
    let address = *(ptr_to_val as *const u64);
    let val_ptr = (address << 32) as *const u8;
    let message = format!("Read: {}", *val_ptr);
    sol_log(message.as_bytes());
    panic!("Should not have reached this stage!");
}

unsafe fn write_to_account(
    current_ix: &InstructionFrame,
    tx_accounts_metadata: &mut [AccountSharedFields],
) {
    let ix_accounts = current_ix.instruction_accounts.deref();
    let ix_account = ix_accounts.get_unchecked(0);

    let account_data = tx_accounts_metadata
        .get_unchecked_mut(ix_account.index_in_transaction as usize)
        .payload
        .deref_mut();
    *account_data.get_unchecked_mut(0) = 7;
    *account_data.get_unchecked_mut(1) = 8;
    *account_data.get_unchecked_mut(2) = 9;
}

unsafe fn test_set_buffer_length_return_scratchpad(write_just_outside: bool) {
    set_buffer_length(0x7_0000_0000u64, 128);
    for i in 0..128 {
        assert_eq!(std::ptr::read((0x7_0000_0000u64 + i) as *const u8), 0);
    }
    let write_offset = if write_just_outside { 128 } else { 127 };
    std::ptr::write((0x7_0000_0000u64 + write_offset) as *mut u8, 42);
    set_buffer_length(0x7_0000_0000, 256);
    assert_eq!(std::ptr::read((0x7_0000_0000u64 + 127) as *const u8), 42);
    for i in 128..256 {
        assert_eq!(std::ptr::read((0x7_0000_0000u64 + i) as *const u8), 0);
    }
}

unsafe fn test_set_buffer_length_account(
    account_idx: u64,
    account_metadata: &[AccountSharedFields],
) {
    let meta = &account_metadata[account_idx as usize];
    assert_eq!(meta.payload.len(), 3);
    let mut expected_data = [0; 6];
    expected_data[..3].copy_from_slice(meta.payload.deref());
    set_buffer_length(meta.payload.ptr(), 6);
    assert_eq!(meta.payload.len(), 6);
    let account_data = core::slice::from_raw_parts(meta.payload.ptr() as *const u8, 6);
    assert_eq!(account_data, expected_data);
}

unsafe fn test_assign_owner(ix_ctx: &InstructionFrame, accounts: &mut [AccountSharedFields]) {
    let ix_accounts = ix_ctx.instruction_accounts.deref();
    let first_account_idx_in_tx = ix_accounts.get_unchecked(0).index_in_transaction as usize;
    let second_account_idx_in_tx = ix_accounts.get_unchecked(1).index_in_transaction as usize;
    let debug_str = format!(
        "lamports: {}",
        accounts.get_unchecked(first_account_idx_in_tx).lamports
    );
    sol_log(debug_str.as_bytes());

    let first_account = accounts.get_unchecked(first_account_idx_in_tx);
    let new_ower = Pubkey::new_from_array(first_account.payload.deref()[0..32].try_into().unwrap());
    let write_to_account_afterwards = first_account.payload.deref()[32];

    // Asserting old owner
    let program_id = accounts
        .get_unchecked(ix_ctx.program_account_index_in_tx as usize)
        .key
        .clone();
    let second_account = accounts.get_unchecked_mut(second_account_idx_in_tx);
    assert_eq!(program_id, second_account.owner);

    assign_owner(second_account_idx_in_tx as u64, &new_ower);

    // Checking new owner
    let second_account = accounts.get_unchecked_mut(second_account_idx_in_tx);
    assert_eq!(second_account.owner, new_ower);

    // I cannot write to the account after changing its owner
    // This write should fail
    if write_to_account_afterwards == 1 {
        *second_account.payload.deref_mut().get_unchecked_mut(0) = 9;
    }
}

#[unsafe(no_mangle)]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn entrypoint() -> u64 {
    // Transaction frame
    let tx_frame_ptr = TRANSACTION_FRAME_ADDRESS as *const TransactionFrame;
    let tx_frame = &*tx_frame_ptr;

    let instruction_trace = slice::from_raw_parts(
        INSTRUCTION_TRACE_AREA as *const InstructionFrame,
        tx_frame.total_number_of_instructions_in_trace as usize,
    );
    let tx_accounts_metadata = slice::from_raw_parts_mut(
        ACCOUNT_METADATA_AREA as *mut AccountSharedFields,
        tx_frame.number_of_transaction_accounts as usize,
    );

    let current_ix = instruction_trace
        .get(tx_frame.current_executing_instruction as usize)
        .unwrap();
    match current_ix.instruction_data.deref() {
        [0x00, ..] => {
            let mes = format!("tx accs: {}", tx_frame.number_of_transaction_accounts);
            sol_log(mes.as_bytes());
            read_invalid_regions(current_ix);
        }
        [0x01, ..] => {
            write_to_account(current_ix, tx_accounts_metadata);
        }
        [0x02, ..] | [0x03, ..] => {
            test_valid_accesses(tx_frame, tx_accounts_metadata);
        }
        [b @ 0x04, ..] | [b @ 0x05, ..] => test_set_buffer_length_return_scratchpad(*b == 0x05),
        [0x06, ..] => test_set_buffer_length_account(1, tx_accounts_metadata),
        [0x07, ..] => test_set_buffer_length_account(3, tx_accounts_metadata),
        [0x08, ..] => test_set_buffer_length_account(2, tx_accounts_metadata),
        [0x09, ..] => test_assign_owner(current_ix, tx_accounts_metadata),
        _ => panic!("unknown command"),
    }
    0
}
