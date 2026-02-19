//! Example Rust-based SBF program that supports the deprecated loader

#![allow(unreachable_code)]
#![allow(clippy::arithmetic_side_effects)]

use {
    solana_account_info::AccountInfo,
    solana_instruction::{AccountMeta, Instruction},
    solana_msg::msg,
    solana_program::{log::sol_log_params, program::invoke},
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
    solana_sbf_rust_invoke_dep::*,
    solana_sbf_rust_realloc_dep::*,
    solana_sdk_ids::bpf_loader,
};

#[derive(Debug, PartialEq)]
struct SStruct {
    x: u64,
    y: u64,
    z: u64,
}

#[inline(never)]
fn return_sstruct() -> SStruct {
    SStruct { x: 1, y: 2, z: 3 }
}

#[no_mangle]
fn custom_panic(info: &core::panic::PanicInfo<'_>) {
    // Full panic reporting
    msg!(&format!("{info}"));
}

solana_program::entrypoint_deprecated!(process_instruction);
fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    msg!("Program identifier:");
    program_id.log();

    assert!(!bpf_loader::check_id(program_id));

    // test_sol_alloc_free_no_longer_deployable calls this program with
    // bpf_loader instead of bpf_loader_deprecated, so instruction_data isn't
    // deserialized correctly and is empty.
    match instruction_data.first() {
        Some(&REALLOC) => {
            let (bytes, _) = instruction_data[2..].split_at(std::mem::size_of::<usize>());
            let new_len = usize::from_le_bytes(bytes.try_into().unwrap());
            msg!("realloc to {}", new_len);
            let account = &accounts[0];
            account.resize(new_len)?;
            assert_eq!(new_len, account.data_len());
        }
        Some(&REALLOC_EXTEND_FROM_SLICE) => {
            msg!("realloc extend from slice deprecated");
            let data = &instruction_data[1..];
            let account = &accounts[0];
            let prev_len = account.data_len();
            account.resize(prev_len + data.len())?;
            account.data.borrow_mut()[prev_len..].copy_from_slice(data);
        }
        Some(&TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS) => {
            msg!("DEPRECATED LOADER TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS");
            const ARGUMENT_INDEX: usize = 1;
            const CALLEE_PROGRAM_INDEX: usize = 3;
            let account = &accounts[ARGUMENT_INDEX];
            let callee_program_id = accounts[CALLEE_PROGRAM_INDEX].key;

            let expected = {
                let data = &instruction_data[1..];
                let prev_len = account.data_len();
                // when stricter_abi_and_runtime_constraints is off, this will accidentally clobber
                // whatever comes after the data slice (owner, executable, rent
                // epoch etc). When stricter_abi_and_runtime_constraints is on, you get an
                // InvalidRealloc error.
                account.resize(prev_len + data.len())?;
                account.data.borrow_mut()[prev_len..].copy_from_slice(data);
                account.data.borrow().to_vec()
            };

            let mut instruction_data = vec![TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_NESTED];
            instruction_data.extend_from_slice(&expected);
            invoke(
                &create_instruction(
                    *callee_program_id,
                    &[
                        (accounts[ARGUMENT_INDEX].key, true, false),
                        (callee_program_id, false, false),
                    ],
                    instruction_data,
                ),
                accounts,
            )
            .unwrap();
        }
        Some(&TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_NESTED) => {
            msg!("DEPRECATED LOADER TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_NESTED");
            const ARGUMENT_INDEX: usize = 0;
            let account = &accounts[ARGUMENT_INDEX];
            assert_eq!(*account.data.borrow(), &instruction_data[1..]);
        }
        Some(&TEST_CPI_ACCOUNT_UPDATE_CALLEE_GROWS) => {
            msg!("DEPRECATED LOADER TEST_CPI_ACCOUNT_UPDATE_CALLEE_GROWS");
            const ARGUMENT_INDEX: usize = 1;
            const REALLOC_PROGRAM_INDEX: usize = 2;
            const INVOKE_PROGRAM_INDEX: usize = 3;
            let account = &accounts[ARGUMENT_INDEX];
            let realloc_program_id = accounts[REALLOC_PROGRAM_INDEX].key;
            let realloc_program_owner = accounts[REALLOC_PROGRAM_INDEX].owner;
            let invoke_program_id = accounts[INVOKE_PROGRAM_INDEX].key;
            let mut instruction_data = instruction_data.to_vec();
            let mut expected = account.data.borrow().to_vec();
            expected.extend_from_slice(&instruction_data[1..]);
            instruction_data[0] = REALLOC_EXTEND_FROM_SLICE;
            invoke(
                &create_instruction(
                    *realloc_program_id,
                    &[
                        (accounts[ARGUMENT_INDEX].key, true, false),
                        (realloc_program_id, false, false),
                        (invoke_program_id, false, false),
                    ],
                    instruction_data.to_vec(),
                ),
                accounts,
            )
            .unwrap();

            if !solana_sdk_ids::bpf_loader_deprecated::check_id(realloc_program_owner) {
                assert_eq!(&*account.data.borrow(), &expected);
            }
        }
        Some(&TEST_CPI_ACCOUNT_UPDATE_CALLEE_SHRINKS_SMALLER_THAN_ORIGINAL_LEN) => {
            msg!(
                "DEPRECATED LOADER \
                 TEST_CPI_ACCOUNT_UPDATE_CALLEE_SHRINKS_SMALLER_THAN_ORIGINAL_LEN"
            );
            const ARGUMENT_INDEX: usize = 1;
            const REALLOC_PROGRAM_INDEX: usize = 2;
            const INVOKE_PROGRAM_INDEX: usize = 3;
            let account = &accounts[ARGUMENT_INDEX];
            let realloc_program_id = accounts[REALLOC_PROGRAM_INDEX].key;
            let realloc_program_owner = accounts[REALLOC_PROGRAM_INDEX].owner;
            let invoke_program_id = accounts[INVOKE_PROGRAM_INDEX].key;
            let stricter_abi_and_runtime_constraints = instruction_data[1];
            let new_len = usize::from_le_bytes(instruction_data[2..10].try_into().unwrap());
            let prev_len = account.data_len();
            let expected = account.data.borrow()[..new_len].to_vec();
            let mut instruction_data = vec![REALLOC, 0];
            instruction_data.extend_from_slice(&new_len.to_le_bytes());
            invoke(
                &create_instruction(
                    *realloc_program_id,
                    &[
                        (accounts[ARGUMENT_INDEX].key, true, false),
                        (realloc_program_id, false, false),
                        (invoke_program_id, false, false),
                    ],
                    instruction_data,
                ),
                accounts,
            )
            .unwrap();

            // deserialize_parameters_for_abiv0 predates realloc support, and
            // hardcodes the account data length to the original length.
            if !solana_program::bpf_loader_deprecated::check_id(realloc_program_owner)
                && stricter_abi_and_runtime_constraints == 0
            {
                assert_eq!(&*account.data.borrow(), &expected);
                assert_eq!(
                    unsafe {
                        std::slice::from_raw_parts(
                            account.data.borrow().as_ptr().add(new_len),
                            prev_len - new_len,
                        )
                    },
                    &vec![0; prev_len - new_len]
                );
            }
        }
        Some(&TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS) => {
            msg!("DEPRECATED LOADER TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS");
            const ARGUMENT_INDEX: usize = 1;
            const INVOKE_PROGRAM_INDEX: usize = 3;
            const SENTINEL: u8 = 42;
            let account = &accounts[ARGUMENT_INDEX];
            let invoke_program_id = accounts[INVOKE_PROGRAM_INDEX].key;

            let prev_data = {
                let data = &instruction_data[10..];
                let prev_len = account.data_len();
                account.resize(prev_len + data.len())?;
                account.data.borrow_mut()[prev_len..].copy_from_slice(data);
                unsafe {
                    // write a sentinel value just outside the account data to
                    // check that when CPI zeroes the realloc region it doesn't
                    // zero too much
                    *account
                        .data
                        .borrow_mut()
                        .as_mut_ptr()
                        .add(prev_len + data.len()) = SENTINEL;
                };
                account.data.borrow().to_vec()
            };

            let mut expected = account.data.borrow().to_vec();
            let stricter_abi_and_runtime_constraints = instruction_data[1];
            let new_len = usize::from_le_bytes(instruction_data[2..10].try_into().unwrap());
            expected.extend_from_slice(&instruction_data[10..]);
            let mut instruction_data =
                vec![TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS_NESTED];
            instruction_data.extend_from_slice(&new_len.to_le_bytes());
            invoke(
                &create_instruction(
                    *invoke_program_id,
                    &[
                        (accounts[ARGUMENT_INDEX].key, true, false),
                        (invoke_program_id, false, false),
                    ],
                    instruction_data,
                ),
                accounts,
            )
            .unwrap();

            assert_eq!(*account.data.borrow(), &prev_data[..new_len]);
            if stricter_abi_and_runtime_constraints == 0 {
                assert_eq!(
                    unsafe {
                        std::slice::from_raw_parts(
                            account.data.borrow().as_ptr().add(new_len),
                            prev_data.len() - new_len,
                        )
                    },
                    &vec![0; prev_data.len() - new_len]
                );
                assert_eq!(
                    unsafe { *account.data.borrow().as_ptr().add(prev_data.len()) },
                    SENTINEL
                );
            }
        }
        Some(&TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS_NESTED) => {
            msg!("DEPRECATED LOADER TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS_NESTED");
            const ARGUMENT_INDEX: usize = 0;
            let account = &accounts[ARGUMENT_INDEX];
            let new_len = usize::from_le_bytes(instruction_data[1..9].try_into().unwrap());
            account.resize(new_len).unwrap();
        }
        _ => {
            {
                // Log the provided account keys and instruction input data.  In the case of
                // the no-op program, no account keys or input data are expected but real
                // programs will have specific requirements so they can do their work.
                msg!("Account keys and instruction input data:");
                sol_log_params(accounts, instruction_data);

                // Test - use std methods, unwrap

                // valid bytes, in a stack-allocated array
                let sparkle_heart = [240, 159, 146, 150];
                let result_str = std::str::from_utf8(&sparkle_heart).unwrap();
                assert_eq!(4, result_str.len());
                assert_eq!("💖", result_str);
                msg!(result_str);
            }

            {
                // Test - struct return

                let s = return_sstruct();
                assert_eq!(s.x + s.y + s.z, 6);
            }

            {
                // Test - arch config
                #[cfg(not(target_os = "solana"))]
                panic!();
            }
        }
    }

    Ok(())
}

pub fn create_instruction(
    program_id: Pubkey,
    arguments: &[(&Pubkey, bool, bool)],
    data: Vec<u8>,
) -> Instruction {
    let accounts = arguments
        .iter()
        .map(|(key, is_writable, is_signer)| {
            if *is_writable {
                AccountMeta::new(**key, *is_signer)
            } else {
                AccountMeta::new_readonly(**key, *is_signer)
            }
        })
        .collect();
    Instruction {
        program_id,
        accounts,
        data,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_return_sstruct() {
        assert_eq!(SStruct { x: 1, y: 2, z: 3 }, return_sstruct());
    }
}
