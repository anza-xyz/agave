use {
    solana_program::{
        custom_heap_default, custom_panic_default, entrypoint::SUCCESS, pubkey::Pubkey,
    },
    std::{slice, str::FromStr},
};

const VIRTUAL_ADDRESS_BITS: usize = 32;
const MM_REGION_SIZE: u64 = 1 << VIRTUAL_ADDRESS_BITS;
const SCRATCHPAD_ADDR: u64 = MM_REGION_SIZE * 5;
const CPI_SCRATCHPAD_ADDRESS: u64 = MM_REGION_SIZE * 255;

/// `ReturnDataScratchPad` contains the return data. Programs will be allowed to read and write to
/// the data slice.
#[repr(C)]
struct ReturnDataScratchPad<'a> {
    /// Pubkey of the last program to write to the scratchpad slice.
    pubkey: Pubkey,
    data: &'a [u8],
}

/// `TransactionAccount` is the generic representation for an account in the transaction.
#[repr(C)]
struct TransactionAccount<'a> {
    pubkey: Pubkey,
    owner: Pubkey,
    lamports: u64,
    payload: &'a [u8],
}

/// `ExecutionData` is split from `TransactionContext` to facilitate reading from bytes.
#[repr(C)]
struct ExecutionData<'a> {
    return_data_scratchpad: ReturnDataScratchPad<'a>,
    cpi_scratchpad: &'a [u8],
    /// Index in transaction of the currently executing instruction
    ix_idx: u64,
    /// Number of instructions in transaction
    ix_num: u64,
}

/// `TransactionContext` contains the information extracted from the transaction area
#[repr(C)]
struct TransactionContext<'a> {
    execution_data: &'a ExecutionData<'a>,
    accounts: &'a [TransactionAccount<'a>],
}

impl TransactionContext<'_> {
    fn new(mut addr: *const u8) -> TransactionContext<'static> {
        let data = unsafe { &*(addr as *const ExecutionData) };

        let accounts = unsafe {
            addr = addr.add(size_of::<ExecutionData>());
            let accounts_no = *(addr as *const u64);
            addr = addr.add(size_of::<u64>());
            slice::from_raw_parts(addr as *const TransactionAccount, accounts_no as usize)
        };

        TransactionContext {
            execution_data: data,
            accounts,
        }
    }
}

#[no_mangle]
pub extern "C" fn entrypoint(input: *mut u8) -> u64 {
    let ctx = TransactionContext::new(input);
    let execution_data = ctx.execution_data;

    assert_eq!(
        execution_data.return_data_scratchpad.pubkey.to_bytes(),
        [0; 32]
    );
    assert_eq!(
        execution_data.return_data_scratchpad.data.as_ptr() as usize,
        SCRATCHPAD_ADDR as usize
    );

    assert_eq!(
        execution_data.cpi_scratchpad.as_ptr() as usize,
        CPI_SCRATCHPAD_ADDRESS as usize
    );
    assert_eq!(execution_data.cpi_scratchpad.len(), 0);

    assert_eq!(execution_data.ix_idx, 0);
    assert_eq!(execution_data.ix_num, 1);

    assert_eq!(ctx.accounts.len(), 3);
    // The first account is the mint account.
    // The address is not known by the program.
    assert_eq!(ctx.accounts[0].owner.to_bytes(), [0; 32]);
    assert_eq!(ctx.accounts[0].payload.as_ptr() as u64, MM_REGION_SIZE * 8);
    assert_eq!(ctx.accounts[0].payload.len(), 0);

    let mut key_arr = [0u8; 32];
    for (i, item) in key_arr.iter_mut().enumerate() {
        *item = i as u8;
    }

    // The second account is a mocked one
    assert_eq!(ctx.accounts[1].pubkey.to_bytes(), key_arr);
    for (i, item) in key_arr.iter_mut().enumerate() {
        *item = 2usize.saturating_mul(i) as u8;
    }
    assert_eq!(ctx.accounts[1].owner.to_bytes(), key_arr);
    assert_eq!(ctx.accounts[1].lamports, 789);
    assert_eq!(ctx.accounts[1].payload.as_ptr() as u64, MM_REGION_SIZE * 9);
    assert_eq!(ctx.accounts[1].payload.len(), 5);

    // The third account is the program account. The program id is not yet available for comparison
    let loader_v4_key = Pubkey::from_str("LoaderV411111111111111111111111111111111111").unwrap();
    assert_eq!(ctx.accounts[2].owner, loader_v4_key);
    assert_eq!(ctx.accounts[2].payload.as_ptr() as u64, MM_REGION_SIZE * 10);

    SUCCESS
}

custom_heap_default!();
custom_panic_default!();
