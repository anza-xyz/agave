//! Deserialization Benchmark - Demonstrates current VM deserialization overhead
//! and where register pre-population could help

use {
    solana_account_info::AccountInfo,
    solana_msg::msg,
    solana_program::compute_units::sol_remaining_compute_units,
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
};

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    msg!("=== Deserialization Benchmark Starting ===");
    
    let start_cu = sol_remaining_compute_units();
    msg!("Starting CU: {}", start_cu);

    // This is what programs currently have to do: 
    // repeatedly access account metadata that could be pre-computed
    match instruction_data.first().unwrap_or(&0) {
        0 => benchmark_account_access_patterns(accounts),
        1 => benchmark_repeated_key_access(accounts),
        2 => benchmark_lamports_checks(accounts),
        3 => benchmark_owner_checks(accounts),
        4 => benchmark_data_length_access(accounts),
        _ => msg!("Unknown benchmark mode"),
    }

    let end_cu = sol_remaining_compute_units();
    let consumed = start_cu - end_cu;
    msg!("=== Benchmark Complete ===");
    msg!("CU consumed: {}", consumed);
    msg!("Ending CU: {}", end_cu);

    Ok(())
}

/// Simulates common patterns where programs repeatedly access account metadata
/// This shows the current overhead that could be optimized with register pre-population
fn benchmark_account_access_patterns(accounts: &[AccountInfo]) {
    msg!("Running: Account Access Pattern Benchmark");
    
    if accounts.is_empty() {
        msg!("No accounts provided");
        return;
    }

    let account = &accounts[0];
    
    // Pattern 1: Repeated key access (very common in programs)
    for i in 0..100 {
        let _key = account.key;  // This currently requires VM deserialization each time
        if i % 20 == 0 {
            msg!("Iteration {}: key = {}", i, _key);
        }
    }
    
    // Pattern 2: Lamports checking in loops
    for i in 0..50 {
        let _lamports = account.lamports();  // Another deserialization
        if i % 10 == 0 {
            msg!("Iteration {}: lamports = {}", i, _lamports);
        }
    }
}

/// Benchmarks repeated key access - very common in validation logic
fn benchmark_repeated_key_access(accounts: &[AccountInfo]) {
    msg!("Running: Repeated Key Access Benchmark");
    
    for account in accounts.iter().take(5) {
        // This pattern is extremely common - programs validate keys repeatedly
        for _i in 0..20 {
            let _key = account.key;
            let _is_signer = account.is_signer;
            let _is_writable = account.is_writable;
            // Each access requires going through the VM's deserialization layer
        }
        msg!("Processed account: {}", account.key);
    }
}

/// Benchmarks lamports access patterns
fn benchmark_lamports_checks(accounts: &[AccountInfo]) {
    msg!("Running: Lamports Access Benchmark");
    
    for account in accounts.iter().take(3) {
        let mut total = 0u64;
        // Programs often sum up lamports or do balance checks
        for _i in 0..30 {
            total += account.lamports();  // Each call goes through deserialization
        }
        msg!("Account {} total (30x): {}", account.key, total);
    }
}

/// Benchmarks owner checks - very common in CPI validation
fn benchmark_owner_checks(accounts: &[AccountInfo]) {
    msg!("Running: Owner Check Benchmark");
    
    for account in accounts.iter().take(3) {
        // Programs constantly check owners for security
        for _i in 0..25 {
            let _owner = account.owner;  // Deserialization cost each time
            let _executable = account.executable;
        }
        msg!("Account {} owner: {}", account.key, account.owner);
    }
}

/// Benchmarks data length access
fn benchmark_data_length_access(accounts: &[AccountInfo]) {
    msg!("Running: Data Length Access Benchmark");
    
    for account in accounts.iter().take(3) {
        let mut total_len = 0usize;
        // Programs often check data lengths before processing
        for _i in 0..40 {
            total_len += account.data_len();  // Yet another deserialization
        }
        msg!("Account {} total length (40x): {}", account.key, total_len);
    }
}

/// This function demonstrates what an optimized version might look like
/// if we had registers pre-populated with common account metadata
#[allow(dead_code)]
fn theoretical_optimized_version(accounts: &[AccountInfo]) {
    msg!("=== Theoretical Optimized Version ===");
    
    // In an optimized world, these values would be available in VM registers:
    // r2 = account[0].key (32 bytes)
    // r3 = account[0].lamports (8 bytes) 
    // r4 = account[0].owner (32 bytes)
    // r5 = account[0].data_len (8 bytes)
    // r6 = account[0].is_signer + is_writable + executable (packed flags)
    
    for account in accounts.iter().take(3) {
        // Instead of VM deserialization calls, these would be direct register reads:
        // asm!("mov {}, r2", out(reg) key);
        // asm!("mov {}, r3", out(reg) lamports);
        
        // This would eliminate most of the CU overhead we see in the benchmarks above
        let _key = account.key;
        let _lamports = account.lamports();
        let _owner = account.owner;
    }
    
    msg!("This version would consume far fewer CUs");
} 