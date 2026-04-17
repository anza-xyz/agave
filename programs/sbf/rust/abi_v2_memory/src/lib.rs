use {
    solana_transaction_context::{
        instruction::InstructionFrame,
        transaction::TransactionFrame,
        transaction_accounts::AccountSharedFields,
        vm_addresses::{
            ACCOUNT_METADATA_AREA, GUEST_INSTRUCTION_DATA_BASE_ADDRESS, GUEST_REGION_SIZE,
            INSTRUCTION_TRACE_AREA, RETURN_DATA_SCRATCHPAD, TRANSACTION_FRAME_ADDRESS,
        },
    },
    std::slice,
};

fn sol_log(message: &[u8]) {
    unsafe {
        let syscall: extern "C" fn(*const u8, u64) = core::mem::transmute(544561597u64); // murmur32 hash of "sol_log_"
        syscall(message.as_ptr(), message.len() as u64)
    }
}

#[no_mangle]
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

        assert_eq!(current_ix.instruction_data.deref(), b"IX1");
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

        assert_eq!(current_ix.instruction_data.deref(), b"IX2");
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

#[no_mangle]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn entrypoint() -> u64 {
    // Transaction frame
    let tx_frame_ptr = TRANSACTION_FRAME_ADDRESS as *const TransactionFrame;
    let tx_frame = &*tx_frame_ptr;

    let instruction_trace = slice::from_raw_parts(
        INSTRUCTION_TRACE_AREA as *const InstructionFrame,
        tx_frame.total_number_of_instructions_in_trace as usize,
    );
    let tx_accounts_metadata = slice::from_raw_parts(
        ACCOUNT_METADATA_AREA as *const AccountSharedFields,
        tx_frame.number_of_transaction_accounts as usize,
    );

    let current_ix = instruction_trace
        .get(tx_frame.current_executing_instruction as usize)
        .unwrap();
    if current_ix.instruction_data.deref() == b"IX1"
        || current_ix.instruction_data.deref() == b"IX2"
    {
        test_valid_accesses(tx_frame, tx_accounts_metadata);
    } else if *current_ix.instruction_data.deref().get_unchecked(0) == 0 {
        let mes = format!("tx accs: {}", tx_frame.number_of_transaction_accounts);
        sol_log(mes.as_bytes());
        read_invalid_regions(current_ix);
    }

    0
}