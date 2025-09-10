use {
    super::*,
    solana_instruction::Instruction,
    solana_program_runtime::{
        cpi::{
            check_instruction_size, cpi_common, translate_account_infos,
            translate_and_update_accounts, CallerAccount, SolAccountInfo, SolAccountMeta,
            SolInstruction, SolSignerSeedC, SolSignerSeedsC, SyscallInvokeSigned,
            TranslatedAccount,
        },
        memory::{translate_slice, translate_type},
    },
    solana_stable_layout::stable_instruction::StableInstruction,
};

declare_builtin_function!(
    /// Cross-program invocation called from Rust
    SyscallInvokeSignedRust,
    fn rust(
        invoke_context: &mut InvokeContext,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedRust {
    fn translate_instruction(
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Instruction, Error> {
        let ix = translate_type::<StableInstruction>(memory_mapping, addr, check_aligned)?;
        let account_metas = translate_slice::<AccountMeta>(
            memory_mapping,
            ix.accounts.as_vaddr(),
            ix.accounts.len(),
            check_aligned,
        )?;
        let data = translate_slice::<u8>(
            memory_mapping,
            ix.data.as_vaddr(),
            ix.data.len(),
            check_aligned,
        )?
        .to_vec();

        check_instruction_size(account_metas.len(), data.len())?;

        consume_compute_meter(
            invoke_context,
            (data.len() as u64)
                .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                .unwrap_or(u64::MAX),
        )?;

        let mut accounts = Vec::with_capacity(account_metas.len());
        #[allow(clippy::needless_range_loop)]
        for account_index in 0..account_metas.len() {
            #[allow(clippy::indexing_slicing)]
            let account_meta = &account_metas[account_index];
            if unsafe {
                std::ptr::read_volatile(&account_meta.is_signer as *const _ as *const u8) > 1
                    || std::ptr::read_volatile(&account_meta.is_writable as *const _ as *const u8)
                        > 1
            } {
                return Err(Box::new(InstructionError::InvalidArgument));
            }
            accounts.push(account_meta.clone());
        }

        Ok(Instruction {
            accounts,
            data,
            program_id: ix.program_id,
        })
    }

    fn translate_accounts<'a>(
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping<'_>,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error> {
        let (account_infos, account_info_keys) = translate_account_infos(
            account_infos_addr,
            account_infos_len,
            |account_info: &AccountInfo| account_info.key as *const _ as u64,
            memory_mapping,
            invoke_context,
            check_aligned,
        )?;

        translate_and_update_accounts(
            &account_info_keys,
            account_infos,
            account_infos_addr,
            invoke_context,
            memory_mapping,
            check_aligned,
            CallerAccount::from_account_info,
        )
    }

    fn translate_signers(
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
        check_aligned: bool,
    ) -> Result<Vec<Pubkey>, Error> {
        let mut signers = Vec::new();
        if signers_seeds_len > 0 {
            let signers_seeds = translate_slice::<VmSlice<VmSlice<u8>>>(
                memory_mapping,
                signers_seeds_addr,
                signers_seeds_len,
                check_aligned,
            )?;
            if signers_seeds.len() > MAX_SIGNERS {
                return Err(Box::new(SyscallError::TooManySigners));
            }
            for signer_seeds in signers_seeds.iter() {
                let untranslated_seeds = translate_slice::<VmSlice<u8>>(
                    memory_mapping,
                    signer_seeds.ptr(),
                    signer_seeds.len(),
                    check_aligned,
                )?;
                if untranslated_seeds.len() > MAX_SEEDS {
                    return Err(Box::new(InstructionError::MaxSeedLengthExceeded));
                }
                let seeds = untranslated_seeds
                    .iter()
                    .map(|untranslated_seed| {
                        untranslated_seed.translate(memory_mapping, check_aligned)
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                let signer = Pubkey::create_program_address(&seeds, program_id)
                    .map_err(SyscallError::BadSeeds)?;
                signers.push(signer);
            }
            Ok(signers)
        } else {
            Ok(vec![])
        }
    }
}

declare_builtin_function!(
    /// Cross-program invocation called from C
    SyscallInvokeSignedC,
    fn rust(
        invoke_context: &mut InvokeContext,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedC {
    fn translate_instruction(
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Instruction, Error> {
        let ix_c = translate_type::<SolInstruction>(memory_mapping, addr, check_aligned)?;

        let program_id =
            translate_type::<Pubkey>(memory_mapping, ix_c.program_id_addr, check_aligned)?;
        let account_metas = translate_slice::<SolAccountMeta>(
            memory_mapping,
            ix_c.accounts_addr,
            ix_c.accounts_len,
            check_aligned,
        )?;
        let data =
            translate_slice::<u8>(memory_mapping, ix_c.data_addr, ix_c.data_len, check_aligned)?
                .to_vec();

        check_instruction_size(ix_c.accounts_len as usize, data.len())?;

        consume_compute_meter(
            invoke_context,
            (data.len() as u64)
                .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                .unwrap_or(u64::MAX),
        )?;

        let mut accounts = Vec::with_capacity(ix_c.accounts_len as usize);
        #[allow(clippy::needless_range_loop)]
        for account_index in 0..ix_c.accounts_len as usize {
            #[allow(clippy::indexing_slicing)]
            let account_meta = &account_metas[account_index];
            if unsafe {
                std::ptr::read_volatile(&account_meta.is_signer as *const _ as *const u8) > 1
                    || std::ptr::read_volatile(&account_meta.is_writable as *const _ as *const u8)
                        > 1
            } {
                return Err(Box::new(InstructionError::InvalidArgument));
            }
            let pubkey =
                translate_type::<Pubkey>(memory_mapping, account_meta.pubkey_addr, check_aligned)?;
            accounts.push(AccountMeta {
                pubkey: *pubkey,
                is_signer: account_meta.is_signer,
                is_writable: account_meta.is_writable,
            });
        }

        Ok(Instruction {
            accounts,
            data,
            program_id: *program_id,
        })
    }

    fn translate_accounts<'a>(
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping<'_>,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error> {
        let (account_infos, account_info_keys) = translate_account_infos(
            account_infos_addr,
            account_infos_len,
            |account_info: &SolAccountInfo| account_info.key_addr,
            memory_mapping,
            invoke_context,
            check_aligned,
        )?;

        translate_and_update_accounts(
            &account_info_keys,
            account_infos,
            account_infos_addr,
            invoke_context,
            memory_mapping,
            check_aligned,
            CallerAccount::from_sol_account_info,
        )
    }

    fn translate_signers(
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
        check_aligned: bool,
    ) -> Result<Vec<Pubkey>, Error> {
        if signers_seeds_len > 0 {
            let signers_seeds = translate_slice::<SolSignerSeedsC>(
                memory_mapping,
                signers_seeds_addr,
                signers_seeds_len,
                check_aligned,
            )?;
            if signers_seeds.len() > MAX_SIGNERS {
                return Err(Box::new(SyscallError::TooManySigners));
            }
            Ok(signers_seeds
                .iter()
                .map(|signer_seeds| {
                    let seeds = translate_slice::<SolSignerSeedC>(
                        memory_mapping,
                        signer_seeds.addr,
                        signer_seeds.len,
                        check_aligned,
                    )?;
                    if seeds.len() > MAX_SEEDS {
                        return Err(Box::new(InstructionError::MaxSeedLengthExceeded) as Error);
                    }
                    let seeds_bytes = seeds
                        .iter()
                        .map(|seed| {
                            translate_slice::<u8>(
                                memory_mapping,
                                seed.addr,
                                seed.len,
                                check_aligned,
                            )
                        })
                        .collect::<Result<Vec<_>, Error>>()?;
                    Pubkey::create_program_address(&seeds_bytes, program_id)
                        .map_err(|err| Box::new(SyscallError::BadSeeds(err)) as Error)
                })
                .collect::<Result<Vec<_>, Error>>()?)
        } else {
            Ok(vec![])
        }
    }
}

#[allow(clippy::indexing_slicing)]
#[allow(clippy::arithmetic_side_effects)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::{Account, AccountSharedData, ReadableAccount},
        solana_instruction::Instruction,
        solana_program_runtime::{
            invoke_context::{BpfAllocator, SerializedAccountMetadata, SyscallContext},
            with_mock_invoke_context_with_feature_set,
        },
        solana_sbpf::{
            ebpf::MM_INPUT_START, memory_region::MemoryRegion, program::SBPFVersion, vm::Config,
        },
        solana_sdk_ids::bpf_loader,
        solana_transaction_context::{
            transaction_accounts::TransactionAccount, IndexOfAccount, InstructionAccount,
        },
        std::{
            cell::{Cell, RefCell},
            mem, ptr,
            rc::Rc,
            slice,
        },
    };

    macro_rules! mock_invoke_context {
        ($invoke_context:ident,
         $transaction_context:ident,
         $instruction_data:expr,
         $transaction_accounts:expr,
         $program_account:expr,
         $instruction_accounts:expr) => {
            let instruction_data = $instruction_data;
            let instruction_accounts = $instruction_accounts
                .iter()
                .map(|index_in_transaction| {
                    InstructionAccount::new(
                        *index_in_transaction as IndexOfAccount,
                        false,
                        $transaction_accounts[*index_in_transaction as usize].2,
                    )
                })
                .collect::<Vec<_>>();
            let transaction_accounts = $transaction_accounts
                .into_iter()
                .map(|a| (a.0, a.1))
                .collect::<Vec<TransactionAccount>>();
            let mut feature_set = SVMFeatureSet::all_enabled();
            feature_set.stricter_abi_and_runtime_constraints = false;
            let feature_set = &feature_set;
            with_mock_invoke_context_with_feature_set!(
                $invoke_context,
                $transaction_context,
                feature_set,
                transaction_accounts
            );
            $invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(
                    $program_account,
                    instruction_accounts,
                    instruction_data,
                )
                .unwrap();
            $invoke_context.push().unwrap();
        };
    }

    #[test]
    fn test_translate_instruction() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foo".to_vec());
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let program_id = Pubkey::new_unique();
        let accounts = vec![AccountMeta {
            pubkey: Pubkey::new_unique(),
            is_signer: true,
            is_writable: false,
        }];
        let data = b"ins data".to_vec();
        let vm_addr = MM_INPUT_START;
        let (_mem, region) = MockInstruction {
            program_id,
            accounts: accounts.clone(),
            data: data.clone(),
        }
        .into_region(vm_addr);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(vec![region], &config, SBPFVersion::V3).unwrap();

        let ins = SyscallInvokeSignedRust::translate_instruction(
            vm_addr,
            &memory_mapping,
            &mut invoke_context,
            true, // check_aligned
        )
        .unwrap();
        assert_eq!(ins.program_id, program_id);
        assert_eq!(ins.accounts, accounts);
        assert_eq!(ins.data, data);
    }

    #[test]
    fn test_translate_signers() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foo".to_vec());
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let program_id = Pubkey::new_unique();
        let (derived_key, bump_seed) = Pubkey::find_program_address(&[b"foo"], &program_id);

        let vm_addr = MM_INPUT_START;
        let (_mem, region) = mock_signers(&[b"foo", &[bump_seed]], vm_addr);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(vec![region], &config, SBPFVersion::V3).unwrap();

        let signers = SyscallInvokeSignedRust::translate_signers(
            &program_id,
            vm_addr,
            1,
            &memory_mapping,
            true, // check_aligned
        )
        .unwrap();
        assert_eq!(signers[0], derived_key);
    }

    #[test]
    fn test_translate_accounts_rust() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foobar".to_vec());
        let account = transaction_accounts[1].1.clone();
        let key = transaction_accounts[1].0;
        let original_data_len = account.data().len();

        let vm_addr = MM_INPUT_START;
        let (_mem, region, account_metadata) =
            MockAccountInfo::new(key, &account).into_region(vm_addr);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(vec![region], &config, SBPFVersion::V3).unwrap();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1, 1]
        );

        invoke_context
            .set_syscall_context(SyscallContext {
                allocator: BpfAllocator::new(solana_program_entrypoint::HEAP_LENGTH as u64),
                accounts_metadata: vec![account_metadata],
                trace_log: Vec::new(),
            })
            .unwrap();

        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(
                0,
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(1, false, true),
                ],
                &[],
            )
            .unwrap();
        let accounts = SyscallInvokeSignedRust::translate_accounts(
            vm_addr,
            1,
            &memory_mapping,
            &mut invoke_context,
            true, // check_aligned
        )
        .unwrap();
        assert_eq!(accounts.len(), 1);
        let caller_account = &accounts[0].caller_account;
        assert_eq!(caller_account.serialized_data, account.data());
        assert_eq!(caller_account.original_data_len, original_data_len);
    }

    type TestTransactionAccount = (Pubkey, AccountSharedData, bool);

    fn transaction_with_one_writable_instruction_account(
        data: Vec<u8>,
    ) -> Vec<TestTransactionAccount> {
        let program_id = Pubkey::new_unique();
        let account = AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: program_id,
            executable: false,
            rent_epoch: 100,
        });
        vec![
            (
                program_id,
                AccountSharedData::from(Account {
                    lamports: 0,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 0,
                }),
                false,
            ),
            (Pubkey::new_unique(), account, true),
        ]
    }

    struct MockInstruction {
        program_id: Pubkey,
        accounts: Vec<AccountMeta>,
        data: Vec<u8>,
    }

    impl MockInstruction {
        fn into_region(self, vm_addr: u64) -> (Vec<u8>, MemoryRegion) {
            let accounts_len = mem::size_of::<AccountMeta>() * self.accounts.len();

            let size = mem::size_of::<StableInstruction>() + accounts_len + self.data.len();

            let mut data = vec![0; size];

            let vm_addr = vm_addr as usize;
            let accounts_addr = vm_addr + mem::size_of::<StableInstruction>();
            let data_addr = accounts_addr + accounts_len;

            let ins = Instruction {
                program_id: self.program_id,
                accounts: unsafe {
                    Vec::from_raw_parts(
                        accounts_addr as *mut _,
                        self.accounts.len(),
                        self.accounts.len(),
                    )
                },
                data: unsafe {
                    Vec::from_raw_parts(data_addr as *mut _, self.data.len(), self.data.len())
                },
            };
            let ins = StableInstruction::from(ins);

            unsafe {
                ptr::write_unaligned(data.as_mut_ptr().cast(), ins);
                data[accounts_addr - vm_addr..][..accounts_len].copy_from_slice(
                    slice::from_raw_parts(self.accounts.as_ptr().cast(), accounts_len),
                );
                data[data_addr - vm_addr..].copy_from_slice(&self.data);
            }

            let region = MemoryRegion::new_writable(data.as_mut_slice(), vm_addr as u64);
            (data, region)
        }
    }

    fn mock_signers(signers: &[&[u8]], vm_addr: u64) -> (Vec<u8>, MemoryRegion) {
        let vm_addr = vm_addr as usize;

        // calculate size
        let fat_ptr_size_of_slice = mem::size_of::<&[()]>(); // pointer size + length size
        let singers_length = signers.len();
        let sum_signers_data_length: usize = signers.iter().map(|s| s.len()).sum();

        // init data vec
        let total_size = fat_ptr_size_of_slice
            + singers_length * fat_ptr_size_of_slice
            + sum_signers_data_length;
        let mut data = vec![0; total_size];

        // data is composed by 3 parts
        // A.
        // [ singers address, singers length, ...,
        // B.                                      |
        //                                         signer1 address, signer1 length, signer2 address ...,
        //                                         ^ p1 --->
        // C.                                                                                           |
        //                                                                                              signer1 data, signer2 data, ... ]
        //                                                                                              ^ p2 --->

        // A.
        data[..fat_ptr_size_of_slice / 2]
            .clone_from_slice(&(fat_ptr_size_of_slice + vm_addr).to_le_bytes());
        data[fat_ptr_size_of_slice / 2..fat_ptr_size_of_slice]
            .clone_from_slice(&(singers_length).to_le_bytes());

        // B. + C.
        let (mut p1, mut p2) = (
            fat_ptr_size_of_slice,
            fat_ptr_size_of_slice + singers_length * fat_ptr_size_of_slice,
        );
        for signer in signers.iter() {
            let signer_length = signer.len();

            // B.
            data[p1..p1 + fat_ptr_size_of_slice / 2]
                .clone_from_slice(&(p2 + vm_addr).to_le_bytes());
            data[p1 + fat_ptr_size_of_slice / 2..p1 + fat_ptr_size_of_slice]
                .clone_from_slice(&(signer_length).to_le_bytes());
            p1 += fat_ptr_size_of_slice;

            // C.
            data[p2..p2 + signer_length].clone_from_slice(signer);
            p2 += signer_length;
        }

        let region = MemoryRegion::new_writable(data.as_mut_slice(), vm_addr as u64);
        (data, region)
    }

    struct MockAccountInfo<'a> {
        key: Pubkey,
        is_signer: bool,
        is_writable: bool,
        lamports: u64,
        data: &'a [u8],
        owner: Pubkey,
        executable: bool,
        _unused: u64,
    }

    impl MockAccountInfo<'_> {
        fn new(key: Pubkey, account: &AccountSharedData) -> MockAccountInfo {
            MockAccountInfo {
                key,
                is_signer: false,
                is_writable: false,
                lamports: account.lamports(),
                data: account.data(),
                owner: *account.owner(),
                executable: account.executable(),
                _unused: account.rent_epoch(),
            }
        }

        fn into_region(self, vm_addr: u64) -> (Vec<u8>, MemoryRegion, SerializedAccountMetadata) {
            let size = mem::size_of::<AccountInfo>()
                + mem::size_of::<Pubkey>() * 2
                + mem::size_of::<RcBox<RefCell<&mut u64>>>()
                + mem::size_of::<u64>()
                + mem::size_of::<RcBox<RefCell<&mut [u8]>>>()
                + self.data.len();
            let mut data = vec![0; size];

            let vm_addr = vm_addr as usize;
            let key_addr = vm_addr + mem::size_of::<AccountInfo>();
            let lamports_cell_addr = key_addr + mem::size_of::<Pubkey>();
            let lamports_addr = lamports_cell_addr + mem::size_of::<RcBox<RefCell<&mut u64>>>();
            let owner_addr = lamports_addr + mem::size_of::<u64>();
            let data_cell_addr = owner_addr + mem::size_of::<Pubkey>();
            let data_addr = data_cell_addr + mem::size_of::<RcBox<RefCell<&mut [u8]>>>();

            #[allow(deprecated)]
            #[allow(clippy::used_underscore_binding)]
            let info = AccountInfo {
                key: unsafe { (key_addr as *const Pubkey).as_ref() }.unwrap(),
                is_signer: self.is_signer,
                is_writable: self.is_writable,
                lamports: unsafe {
                    Rc::from_raw((lamports_cell_addr + RcBox::<&mut u64>::VALUE_OFFSET) as *const _)
                },
                data: unsafe {
                    Rc::from_raw((data_cell_addr + RcBox::<&mut [u8]>::VALUE_OFFSET) as *const _)
                },
                owner: unsafe { (owner_addr as *const Pubkey).as_ref() }.unwrap(),
                executable: self.executable,
                _unused: self._unused,
            };

            unsafe {
                ptr::write_unaligned(data.as_mut_ptr().cast(), info);
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + key_addr - vm_addr) as *mut _,
                    self.key,
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + lamports_cell_addr - vm_addr) as *mut _,
                    RcBox::new(RefCell::new((lamports_addr as *mut u64).as_mut().unwrap())),
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + lamports_addr - vm_addr) as *mut _,
                    self.lamports,
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + owner_addr - vm_addr) as *mut _,
                    self.owner,
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + data_cell_addr - vm_addr) as *mut _,
                    RcBox::new(RefCell::new(slice::from_raw_parts_mut(
                        data_addr as *mut u8,
                        self.data.len(),
                    ))),
                );
                data[data_addr - vm_addr..].copy_from_slice(self.data);
            }

            let region = MemoryRegion::new_writable(data.as_mut_slice(), vm_addr as u64);
            (
                data,
                region,
                SerializedAccountMetadata {
                    original_data_len: self.data.len(),
                    vm_key_addr: key_addr as u64,
                    vm_lamports_addr: lamports_addr as u64,
                    vm_owner_addr: owner_addr as u64,
                    vm_data_addr: data_addr as u64,
                },
            )
        }
    }

    #[repr(C)]
    struct RcBox<T> {
        strong: Cell<usize>,
        weak: Cell<usize>,
        value: T,
    }

    impl<T> RcBox<T> {
        const VALUE_OFFSET: usize = mem::size_of::<Cell<usize>>() * 2;
        fn new(value: T) -> RcBox<T> {
            RcBox {
                strong: Cell::new(0),
                weak: Cell::new(0),
                value,
            }
        }
    }
}
