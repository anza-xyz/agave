#![allow(clippy::arithmetic_side_effects)]

use {
    crate::invoke_context::SerializedAccountMetadata,
    solana_instruction::error::InstructionError,
    solana_program_entrypoint::{BPF_ALIGN_OF_U128, MAX_PERMITTED_DATA_INCREASE, NON_DUP_MARKER},
    solana_pubkey::Pubkey,
    solana_sbpf::{
        aligned_memory::{AlignedMemory, Pod},
        ebpf::{HOST_ALIGN, MM_INPUT_START},
        memory_region::MemoryRegion,
    },
    solana_sdk_ids::bpf_loader_deprecated,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    solana_transaction_context::{
        instruction::InstructionContext, instruction_accounts::BorrowedInstructionAccount,
        IndexOfAccount, MAX_ACCOUNTS_PER_INSTRUCTION,
    },
    std::mem::{self, size_of},
};

/// Modifies the memory mapping in serialization and CPI return for stricter_abi_and_runtime_constraints
pub fn modify_memory_region_of_account(
    account: &mut BorrowedInstructionAccount<'_, '_>,
    region: &mut MemoryRegion,
) {
    region.len = account.get_data().len() as u64;
    if account.can_data_be_changed().is_ok() {
        region.writable = true;
        region.access_violation_handler_payload = Some(account.get_index_in_transaction());
    } else {
        region.writable = false;
        region.access_violation_handler_payload = None;
    }
}

/// Creates the memory mapping in serialization and CPI return for account_data_direct_mapping
pub fn create_memory_region_of_account(
    account: &mut BorrowedInstructionAccount<'_, '_>,
    vaddr: u64,
) -> Result<MemoryRegion, InstructionError> {
    let can_data_be_changed = account.can_data_be_changed().is_ok();
    let mut memory_region = if can_data_be_changed && !account.is_shared() {
        MemoryRegion::new_writable(account.get_data_mut()?, vaddr)
    } else {
        MemoryRegion::new_readonly(account.get_data(), vaddr)
    };
    if can_data_be_changed {
        memory_region.access_violation_handler_payload = Some(account.get_index_in_transaction());
    }
    Ok(memory_region)
}

#[allow(dead_code)]
enum SerializeAccount<'a, 'ix_data> {
    Account(IndexOfAccount, BorrowedInstructionAccount<'a, 'ix_data>),
    Duplicate(IndexOfAccount),
}

struct Serializer {
    buffer: AlignedMemory<HOST_ALIGN>,
    regions: Vec<MemoryRegion>,
    vaddr: u64,
    region_start: usize,
    is_loader_v1: bool,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
}

impl Serializer {
    fn new(
        size: usize,
        start_addr: u64,
        is_loader_v1: bool,
        stricter_abi_and_runtime_constraints: bool,
        account_data_direct_mapping: bool,
    ) -> Serializer {
        Serializer {
            buffer: AlignedMemory::with_capacity(size),
            regions: Vec::new(),
            region_start: 0,
            vaddr: start_addr,
            is_loader_v1,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
        }
    }

    fn fill_write(&mut self, num: usize, value: u8) -> std::io::Result<()> {
        self.buffer.fill_write(num, value)
    }

    fn write<T: Pod>(&mut self, value: T) -> u64 {
        self.debug_assert_alignment::<T>();
        let vaddr = self
            .vaddr
            .saturating_add(self.buffer.len() as u64)
            .saturating_sub(self.region_start as u64);
        // Safety:
        // in serialize_parameters_(aligned|unaligned) first we compute the
        // required size then we write into the newly allocated buffer. There's
        // no need to check bounds at every write.
        //
        // AlignedMemory::write_unchecked _does_ debug_assert!() that the capacity
        // is enough, so in the unlikely case we introduce a bug in the size
        // computation, tests will abort.
        unsafe {
            self.buffer.write_unchecked(value);
        }

        vaddr
    }

    fn write_all(&mut self, value: &[u8]) -> u64 {
        let vaddr = self
            .vaddr
            .saturating_add(self.buffer.len() as u64)
            .saturating_sub(self.region_start as u64);
        // Safety:
        // see write() - the buffer is guaranteed to be large enough
        unsafe {
            self.buffer.write_all_unchecked(value);
        }

        vaddr
    }

    fn write_account(
        &mut self,
        account: &mut BorrowedInstructionAccount<'_, '_>,
    ) -> Result<u64, InstructionError> {
        if !self.stricter_abi_and_runtime_constraints {
            let vm_data_addr = self.vaddr.saturating_add(self.buffer.len() as u64);
            self.write_all(account.get_data());
            if !self.is_loader_v1 {
                let align_offset =
                    (account.get_data().len() as *const u8).align_offset(BPF_ALIGN_OF_U128);
                self.fill_write(MAX_PERMITTED_DATA_INCREASE + align_offset, 0)
                    .map_err(|_| InstructionError::InvalidArgument)?;
            }
            Ok(vm_data_addr)
        } else {
            self.push_region();
            let vm_data_addr = self.vaddr;
            if !self.account_data_direct_mapping {
                self.write_all(account.get_data());
                if !self.is_loader_v1 {
                    self.fill_write(MAX_PERMITTED_DATA_INCREASE, 0)
                        .map_err(|_| InstructionError::InvalidArgument)?;
                }
            }
            let address_space_reserved_for_account = if !self.is_loader_v1 {
                account
                    .get_data()
                    .len()
                    .saturating_add(MAX_PERMITTED_DATA_INCREASE)
            } else {
                account.get_data().len()
            };
            if address_space_reserved_for_account > 0 {
                if !self.account_data_direct_mapping {
                    self.push_region();
                    let region = self.regions.last_mut().unwrap();
                    modify_memory_region_of_account(account, region);
                } else {
                    let new_region = create_memory_region_of_account(account, self.vaddr)?;
                    self.vaddr += address_space_reserved_for_account as u64;
                    self.regions.push(new_region);
                }
            }
            if !self.is_loader_v1 {
                let align_offset =
                    (account.get_data().len() as *const u8).align_offset(BPF_ALIGN_OF_U128);
                if !self.account_data_direct_mapping {
                    self.fill_write(align_offset, 0)
                        .map_err(|_| InstructionError::InvalidArgument)?;
                } else {
                    // The deserialization code is going to align the vm_addr to
                    // BPF_ALIGN_OF_U128. Always add one BPF_ALIGN_OF_U128 worth of
                    // padding and shift the start of the next region, so that once
                    // vm_addr is aligned, the corresponding host_addr is aligned
                    // too.
                    self.fill_write(BPF_ALIGN_OF_U128, 0)
                        .map_err(|_| InstructionError::InvalidArgument)?;
                    self.region_start += BPF_ALIGN_OF_U128.saturating_sub(align_offset);
                }
            }
            Ok(vm_data_addr)
        }
    }

    fn push_region(&mut self) {
        let range = self.region_start..self.buffer.len();
        self.regions.push(MemoryRegion::new_writable(
            self.buffer.as_slice_mut().get_mut(range.clone()).unwrap(),
            self.vaddr,
        ));
        self.region_start = range.end;
        self.vaddr += range.len() as u64;
    }

    fn finish(mut self) -> (AlignedMemory<HOST_ALIGN>, Vec<MemoryRegion>) {
        self.push_region();
        debug_assert_eq!(self.region_start, self.buffer.len());
        (self.buffer, self.regions)
    }

    fn debug_assert_alignment<T>(&self) {
        debug_assert!(
            self.is_loader_v1
                || self
                    .buffer
                    .as_slice()
                    .as_ptr_range()
                    .end
                    .align_offset(mem::align_of::<T>())
                    == 0
        );
    }
}

pub fn serialize_parameters(
    instruction_context: &InstructionContext,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
    mask_out_rent_epoch_in_vm_serialization: bool,
) -> Result<
    (
        AlignedMemory<HOST_ALIGN>,
        Vec<MemoryRegion>,
        Vec<SerializedAccountMetadata>,
        usize,
    ),
    InstructionError,
> {
    let num_ix_accounts = instruction_context.get_number_of_instruction_accounts();
    if num_ix_accounts > MAX_ACCOUNTS_PER_INSTRUCTION as IndexOfAccount {
        return Err(InstructionError::MaxAccountsExceeded);
    }

    let program_id = *instruction_context.get_program_key()?;
    let is_loader_deprecated =
        instruction_context.get_program_owner()? == bpf_loader_deprecated::id();

    let accounts = (0..instruction_context.get_number_of_instruction_accounts())
        .map(|instruction_account_index| {
            if let Some(index) = instruction_context
                .is_instruction_account_duplicate(instruction_account_index)
                .unwrap()
            {
                SerializeAccount::Duplicate(index)
            } else {
                let account = instruction_context
                    .try_borrow_instruction_account(instruction_account_index)
                    .unwrap();
                SerializeAccount::Account(instruction_account_index, account)
            }
        })
        // fun fact: jemalloc is good at caching tiny allocations like this one,
        // so collecting here is actually faster than passing the iterator
        // around, since the iterator does the work to produce its items each
        // time it's iterated on.
        .collect::<Vec<_>>();

    if is_loader_deprecated {
        serialize_parameters_unaligned(
            accounts,
            instruction_context.get_instruction_data(),
            &program_id,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
            mask_out_rent_epoch_in_vm_serialization,
        )
    } else {
        serialize_parameters_aligned(
            accounts,
            instruction_context.get_instruction_data(),
            &program_id,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
            mask_out_rent_epoch_in_vm_serialization,
        )
    }
}

pub fn deserialize_parameters(
    instruction_context: &InstructionContext,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
    buffer: &[u8],
    accounts_metadata: &[SerializedAccountMetadata],
) -> Result<(), InstructionError> {
    let is_loader_deprecated =
        instruction_context.get_program_owner()? == bpf_loader_deprecated::id();
    let account_lengths = accounts_metadata.iter().map(|a| a.original_data_len);
    if is_loader_deprecated {
        deserialize_parameters_unaligned(
            instruction_context,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
            buffer,
            account_lengths,
        )
    } else {
        deserialize_parameters_aligned(
            instruction_context,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
            buffer,
            account_lengths,
        )
    }
}

fn serialize_parameters_unaligned(
    accounts: Vec<SerializeAccount>,
    instruction_data: &[u8],
    program_id: &Pubkey,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
    mask_out_rent_epoch_in_vm_serialization: bool,
) -> Result<
    (
        AlignedMemory<HOST_ALIGN>,
        Vec<MemoryRegion>,
        Vec<SerializedAccountMetadata>,
        usize,
    ),
    InstructionError,
> {
    // Calculate size in order to alloc once
    let mut size = size_of::<u64>();
    for account in &accounts {
        size += 1; // dup
        match account {
            SerializeAccount::Duplicate(_) => {}
            SerializeAccount::Account(_, account) => {
                size += size_of::<u8>() // is_signer
                + size_of::<u8>() // is_writable
                + size_of::<Pubkey>() // key
                + size_of::<u64>()  // lamports
                + size_of::<u64>()  // data len
                + size_of::<Pubkey>() // owner
                + size_of::<u8>() // executable
                + size_of::<u64>(); // rent_epoch
                if !(stricter_abi_and_runtime_constraints && account_data_direct_mapping) {
                    size += account.get_data().len();
                }
            }
        }
    }
    size += size_of::<u64>() // instruction data len
         + instruction_data.len() // instruction data
         + size_of::<Pubkey>(); // program id

    let mut s = Serializer::new(
        size,
        MM_INPUT_START,
        true,
        stricter_abi_and_runtime_constraints,
        account_data_direct_mapping,
    );

    let mut accounts_metadata: Vec<SerializedAccountMetadata> = Vec::with_capacity(accounts.len());
    s.write::<u64>((accounts.len() as u64).to_le());
    for account in accounts {
        match account {
            SerializeAccount::Duplicate(position) => {
                accounts_metadata.push(accounts_metadata.get(position as usize).unwrap().clone());
                s.write(position as u8);
            }
            SerializeAccount::Account(_, mut account) => {
                s.write::<u8>(NON_DUP_MARKER);
                s.write::<u8>(account.is_signer() as u8);
                s.write::<u8>(account.is_writable() as u8);
                let vm_key_addr = s.write_all(account.get_key().as_ref());
                let vm_lamports_addr = s.write::<u64>(account.get_lamports().to_le());
                s.write::<u64>((account.get_data().len() as u64).to_le());
                let vm_data_addr = s.write_account(&mut account)?;
                let vm_owner_addr = s.write_all(account.get_owner().as_ref());
                #[allow(deprecated)]
                s.write::<u8>(account.is_executable() as u8);
                let rent_epoch = if mask_out_rent_epoch_in_vm_serialization {
                    u64::MAX
                } else {
                    account.get_rent_epoch()
                };
                s.write::<u64>(rent_epoch.to_le());
                accounts_metadata.push(SerializedAccountMetadata {
                    original_data_len: account.get_data().len(),
                    vm_key_addr,
                    vm_lamports_addr,
                    vm_owner_addr,
                    vm_data_addr,
                });
            }
        };
    }
    s.write::<u64>((instruction_data.len() as u64).to_le());
    let instruction_data_offset = s.write_all(instruction_data);
    s.write_all(program_id.as_ref());

    let (mem, regions) = s.finish();
    Ok((
        mem,
        regions,
        accounts_metadata,
        instruction_data_offset as usize,
    ))
}

fn deserialize_parameters_unaligned<I: IntoIterator<Item = usize>>(
    instruction_context: &InstructionContext,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
    buffer: &[u8],
    account_lengths: I,
) -> Result<(), InstructionError> {
    let mut start = size_of::<u64>(); // number of accounts
    for (instruction_account_index, pre_len) in (0..instruction_context
        .get_number_of_instruction_accounts())
        .zip(account_lengths.into_iter())
    {
        let duplicate =
            instruction_context.is_instruction_account_duplicate(instruction_account_index)?;
        start += 1; // is_dup
        if duplicate.is_none() {
            let mut borrowed_account =
                instruction_context.try_borrow_instruction_account(instruction_account_index)?;
            start += size_of::<u8>(); // is_signer
            start += size_of::<u8>(); // is_writable
            start += size_of::<Pubkey>(); // key
            let lamports = buffer
                .get(start..start.saturating_add(8))
                .map(<[u8; 8]>::try_from)
                .and_then(Result::ok)
                .map(u64::from_le_bytes)
                .ok_or(InstructionError::InvalidArgument)?;
            if borrowed_account.get_lamports() != lamports {
                borrowed_account.set_lamports(lamports)?;
            }
            start += size_of::<u64>() // lamports
                + size_of::<u64>(); // data length
            if !stricter_abi_and_runtime_constraints {
                let data = buffer
                    .get(start..start + pre_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                // The redundant check helps to avoid the expensive data comparison if we can
                match borrowed_account.can_data_be_resized(pre_len) {
                    Ok(()) => borrowed_account.set_data_from_slice(data)?,
                    Err(err) if borrowed_account.get_data() != data => return Err(err),
                    _ => {}
                }
            } else if !account_data_direct_mapping && borrowed_account.can_data_be_changed().is_ok()
            {
                let data = buffer
                    .get(start..start + pre_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                borrowed_account.set_data_from_slice(data)?;
            } else if borrowed_account.get_data().len() != pre_len {
                borrowed_account.set_data_length(pre_len)?;
            }
            if !(stricter_abi_and_runtime_constraints && account_data_direct_mapping) {
                start += pre_len; // data
            }
            start += size_of::<Pubkey>() // owner
                + size_of::<u8>() // executable
                + size_of::<u64>(); // rent_epoch
        }
    }
    Ok(())
}

fn serialize_parameters_aligned(
    accounts: Vec<SerializeAccount>,
    instruction_data: &[u8],
    program_id: &Pubkey,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
    mask_out_rent_epoch_in_vm_serialization: bool,
) -> Result<
    (
        AlignedMemory<HOST_ALIGN>,
        Vec<MemoryRegion>,
        Vec<SerializedAccountMetadata>,
        usize,
    ),
    InstructionError,
> {
    let mut accounts_metadata = Vec::with_capacity(accounts.len());
    // Calculate size in order to alloc once
    let mut size = size_of::<u64>();
    for account in &accounts {
        size += 1; // dup
        match account {
            SerializeAccount::Duplicate(_) => size += 7, // padding to 64-bit aligned
            SerializeAccount::Account(_, account) => {
                let data_len = account.get_data().len();
                size += size_of::<u8>() // is_signer
                + size_of::<u8>() // is_writable
                + size_of::<u8>() // executable
                + size_of::<u32>() // original_data_len
                + size_of::<Pubkey>()  // key
                + size_of::<Pubkey>() // owner
                + size_of::<u64>()  // lamports
                + size_of::<u64>()  // data len
                + size_of::<u64>(); // rent epoch
                if !(stricter_abi_and_runtime_constraints && account_data_direct_mapping) {
                    size += data_len
                        + MAX_PERMITTED_DATA_INCREASE
                        + (data_len as *const u8).align_offset(BPF_ALIGN_OF_U128);
                } else {
                    size += BPF_ALIGN_OF_U128;
                }
            }
        }
    }
    size += size_of::<u64>() // data len
    + instruction_data.len()
    + size_of::<Pubkey>(); // program id;

    let mut s = Serializer::new(
        size,
        MM_INPUT_START,
        false,
        stricter_abi_and_runtime_constraints,
        account_data_direct_mapping,
    );

    // Serialize into the buffer
    s.write::<u64>((accounts.len() as u64).to_le());
    for account in accounts {
        match account {
            SerializeAccount::Account(_, mut borrowed_account) => {
                s.write::<u8>(NON_DUP_MARKER);
                s.write::<u8>(borrowed_account.is_signer() as u8);
                s.write::<u8>(borrowed_account.is_writable() as u8);
                #[allow(deprecated)]
                s.write::<u8>(borrowed_account.is_executable() as u8);
                s.write_all(&[0u8, 0, 0, 0]);
                let vm_key_addr = s.write_all(borrowed_account.get_key().as_ref());
                let vm_owner_addr = s.write_all(borrowed_account.get_owner().as_ref());
                let vm_lamports_addr = s.write::<u64>(borrowed_account.get_lamports().to_le());
                s.write::<u64>((borrowed_account.get_data().len() as u64).to_le());
                let vm_data_addr = s.write_account(&mut borrowed_account)?;
                let rent_epoch = if mask_out_rent_epoch_in_vm_serialization {
                    u64::MAX
                } else {
                    borrowed_account.get_rent_epoch()
                };
                s.write::<u64>(rent_epoch.to_le());
                accounts_metadata.push(SerializedAccountMetadata {
                    original_data_len: borrowed_account.get_data().len(),
                    vm_key_addr,
                    vm_owner_addr,
                    vm_lamports_addr,
                    vm_data_addr,
                });
            }
            SerializeAccount::Duplicate(position) => {
                accounts_metadata.push(accounts_metadata.get(position as usize).unwrap().clone());
                s.write::<u8>(position as u8);
                s.write_all(&[0u8, 0, 0, 0, 0, 0, 0]);
            }
        };
    }
    s.write::<u64>((instruction_data.len() as u64).to_le());
    let instruction_data_offset = s.write_all(instruction_data);
    s.write_all(program_id.as_ref());

    let (mem, regions) = s.finish();
    Ok((
        mem,
        regions,
        accounts_metadata,
        instruction_data_offset as usize,
    ))
}

fn deserialize_parameters_aligned<I: IntoIterator<Item = usize>>(
    instruction_context: &InstructionContext,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
    buffer: &[u8],
    account_lengths: I,
) -> Result<(), InstructionError> {
    let mut start = size_of::<u64>(); // number of accounts
    for (instruction_account_index, pre_len) in (0..instruction_context
        .get_number_of_instruction_accounts())
        .zip(account_lengths.into_iter())
    {
        let duplicate =
            instruction_context.is_instruction_account_duplicate(instruction_account_index)?;
        start += size_of::<u8>(); // position
        if duplicate.is_some() {
            start += 7; // padding to 64-bit aligned
        } else {
            let mut borrowed_account =
                instruction_context.try_borrow_instruction_account(instruction_account_index)?;
            start += size_of::<u8>() // is_signer
                + size_of::<u8>() // is_writable
                + size_of::<u8>() // executable
                + size_of::<u32>() // original_data_len
                + size_of::<Pubkey>(); // key
            let owner = buffer
                .get(start..start + size_of::<Pubkey>())
                .ok_or(InstructionError::InvalidArgument)?;
            start += size_of::<Pubkey>(); // owner
            let lamports = buffer
                .get(start..start.saturating_add(8))
                .map(<[u8; 8]>::try_from)
                .and_then(Result::ok)
                .map(u64::from_le_bytes)
                .ok_or(InstructionError::InvalidArgument)?;
            if borrowed_account.get_lamports() != lamports {
                borrowed_account.set_lamports(lamports)?;
            }
            start += size_of::<u64>(); // lamports
            let post_len = buffer
                .get(start..start.saturating_add(8))
                .map(<[u8; 8]>::try_from)
                .and_then(Result::ok)
                .map(u64::from_le_bytes)
                .ok_or(InstructionError::InvalidArgument)? as usize;
            start += size_of::<u64>(); // data length
            if post_len.saturating_sub(pre_len) > MAX_PERMITTED_DATA_INCREASE
                || post_len > MAX_PERMITTED_DATA_LENGTH as usize
            {
                return Err(InstructionError::InvalidRealloc);
            }
            if !stricter_abi_and_runtime_constraints {
                let data = buffer
                    .get(start..start + post_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                // The redundant check helps to avoid the expensive data comparison if we can
                match borrowed_account.can_data_be_resized(post_len) {
                    Ok(()) => borrowed_account.set_data_from_slice(data)?,
                    Err(err) if borrowed_account.get_data() != data => return Err(err),
                    _ => {}
                }
            } else if !account_data_direct_mapping && borrowed_account.can_data_be_changed().is_ok()
            {
                let data = buffer
                    .get(start..start + post_len)
                    .ok_or(InstructionError::InvalidArgument)?;
                borrowed_account.set_data_from_slice(data)?;
            } else if borrowed_account.get_data().len() != post_len {
                borrowed_account.set_data_length(post_len)?;
            }
            start += if !(stricter_abi_and_runtime_constraints && account_data_direct_mapping) {
                let alignment_offset = (pre_len as *const u8).align_offset(BPF_ALIGN_OF_U128);
                pre_len // data
                    .saturating_add(MAX_PERMITTED_DATA_INCREASE) // realloc padding
                    .saturating_add(alignment_offset)
            } else {
                // See Serializer::write_account() as to why we have this
                BPF_ALIGN_OF_U128
            };
            start += size_of::<u64>(); // rent_epoch
            if borrowed_account.get_owner().to_bytes() != owner {
                // Change the owner at the end so that we are allowed to change the lamports and data before
                borrowed_account.set_owner(owner)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use {
        super::*,
        crate::with_mock_invoke_context,
        solana_account::{Account, AccountSharedData, ReadableAccount},
        solana_account_info::AccountInfo,
        solana_program_entrypoint::deserialize,
        solana_rent::Rent,
        solana_sbpf::{memory_region::MemoryMapping, program::SBPFVersion, vm::Config},
        solana_sdk_ids::bpf_loader,
        solana_system_interface::MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
        solana_transaction_context::{
            instruction_accounts::InstructionAccount, TransactionContext,
            MAX_ACCOUNTS_PER_TRANSACTION,
        },
        std::{
            borrow::Cow,
            cell::RefCell,
            mem::transmute,
            rc::Rc,
            slice::{self, from_raw_parts, from_raw_parts_mut},
        },
    };

    fn deduplicated_instruction_accounts(
        transaction_indexes: &[IndexOfAccount],
        is_writable: fn(usize) -> bool,
    ) -> Vec<InstructionAccount> {
        transaction_indexes
            .iter()
            .enumerate()
            .map(|(index_in_instruction, index_in_transaction)| {
                InstructionAccount::new(
                    *index_in_transaction,
                    false,
                    is_writable(index_in_instruction),
                )
            })
            .collect()
    }

    #[test]
    fn test_serialize_parameters_with_many_accounts() {
        struct TestCase {
            num_ix_accounts: usize,
            append_dup_account: bool,
            expected_err: Option<InstructionError>,
            name: &'static str,
        }

        for stricter_abi_and_runtime_constraints in [false, true] {
            for TestCase {
                num_ix_accounts,
                append_dup_account,
                expected_err,
                name,
            } in [
                TestCase {
                    name: "serialize max accounts with cap",
                    num_ix_accounts: MAX_ACCOUNTS_PER_INSTRUCTION,
                    append_dup_account: false,
                    expected_err: None,
                },
                TestCase {
                    name: "serialize too many accounts with cap",
                    num_ix_accounts: MAX_ACCOUNTS_PER_INSTRUCTION + 1,
                    append_dup_account: false,
                    expected_err: Some(InstructionError::MaxAccountsExceeded),
                },
                TestCase {
                    name: "serialize too many accounts and append dup with cap",
                    num_ix_accounts: MAX_ACCOUNTS_PER_INSTRUCTION,
                    append_dup_account: true,
                    expected_err: Some(InstructionError::MaxAccountsExceeded),
                },
            ] {
                let program_id = solana_pubkey::new_rand();
                let mut transaction_accounts = vec![(
                    program_id,
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                )];
                for _ in 0..num_ix_accounts {
                    transaction_accounts.push((
                        Pubkey::new_unique(),
                        AccountSharedData::from(Account {
                            lamports: 0,
                            data: vec![],
                            owner: program_id,
                            executable: false,
                            rent_epoch: 0,
                        }),
                    ));
                }

                let transaction_accounts_indexes: Vec<IndexOfAccount> =
                    (0..num_ix_accounts as u16).collect();
                let mut instruction_accounts =
                    deduplicated_instruction_accounts(&transaction_accounts_indexes, |_| false);
                if append_dup_account {
                    instruction_accounts.push(instruction_accounts.last().cloned().unwrap());
                }
                let instruction_data = vec![];

                with_mock_invoke_context!(
                    invoke_context,
                    transaction_context,
                    transaction_accounts
                );
                if instruction_accounts.len() > MAX_ACCOUNTS_PER_INSTRUCTION {
                    // Special case implementation of configure_next_instruction_for_tests()
                    // which avoids the overflow when constructing the dedup_map
                    // by simply not filling it.
                    let dedup_map = vec![u8::MAX; MAX_ACCOUNTS_PER_TRANSACTION];
                    invoke_context
                        .transaction_context
                        .configure_next_instruction(
                            0,
                            instruction_accounts,
                            dedup_map,
                            Cow::Owned(instruction_data.clone()),
                        )
                        .unwrap();
                } else {
                    invoke_context
                        .transaction_context
                        .configure_next_instruction_for_tests(
                            0,
                            instruction_accounts,
                            instruction_data.clone(),
                        )
                        .unwrap();
                }
                invoke_context.push().unwrap();
                let instruction_context = invoke_context
                    .transaction_context
                    .get_current_instruction_context()
                    .unwrap();

                let serialization_result = serialize_parameters(
                    &instruction_context,
                    stricter_abi_and_runtime_constraints,
                    false, // account_data_direct_mapping
                    true,  // mask_out_rent_epoch_in_vm_serialization
                );
                assert_eq!(
                    serialization_result.as_ref().err(),
                    expected_err.as_ref(),
                    "{name} test case failed",
                );
                if expected_err.is_some() {
                    continue;
                }

                let (mut serialized, regions, _account_lengths, _instruction_data_offset) =
                    serialization_result.unwrap();
                let mut serialized_regions = concat_regions(&regions);
                let (de_program_id, de_accounts, de_instruction_data) = unsafe {
                    deserialize(
                        if !stricter_abi_and_runtime_constraints {
                            serialized.as_slice_mut()
                        } else {
                            serialized_regions.as_slice_mut()
                        }
                        .first_mut()
                        .unwrap() as *mut u8,
                    )
                };
                assert_eq!(de_program_id, &program_id);
                assert_eq!(de_instruction_data, &instruction_data);
                for account_info in de_accounts {
                    let index_in_transaction = invoke_context
                        .transaction_context
                        .find_index_of_account(account_info.key)
                        .unwrap();
                    let account = invoke_context
                        .transaction_context
                        .accounts()
                        .try_borrow(index_in_transaction)
                        .unwrap();
                    assert_eq!(account.lamports(), account_info.lamports());
                    assert_eq!(account.data(), &account_info.data.borrow()[..]);
                    assert_eq!(account.owner(), account_info.owner);
                    assert_eq!(account.executable(), account_info.executable);
                    #[allow(deprecated)]
                    {
                        // Using the sdk entrypoint, the rent-epoch is skipped
                        assert_eq!(0, account_info._unused);
                    }
                }
            }
        }
    }

    #[test]
    fn test_serialize_parameters() {
        for stricter_abi_and_runtime_constraints in [false, true] {
            let program_id = solana_pubkey::new_rand();
            let transaction_accounts = vec![
                (
                    program_id,
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 1,
                        data: vec![1u8, 2, 3, 4, 5],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 2,
                        data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 200,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 3,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 3100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 4,
                        data: vec![1u8, 2, 3, 4, 5],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 5,
                        data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 200,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 6,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 3100,
                    }),
                ),
                (
                    program_id,
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader_deprecated::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                ),
            ];
            let instruction_accounts =
                deduplicated_instruction_accounts(&[1, 1, 2, 3, 4, 4, 5, 6], |index| index >= 4);
            let instruction_data = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
            let original_accounts = transaction_accounts.clone();
            with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
            invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(
                    0,
                    instruction_accounts.clone(),
                    instruction_data.clone(),
                )
                .unwrap();
            invoke_context.push().unwrap();
            let instruction_context = invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();

            // check serialize_parameters_aligned
            let (mut serialized, regions, accounts_metadata, _instruction_data_offset) =
                serialize_parameters(
                    &instruction_context,
                    stricter_abi_and_runtime_constraints,
                    false, // account_data_direct_mapping
                    true,  // mask_out_rent_epoch_in_vm_serialization
                )
                .unwrap();

            let mut serialized_regions = concat_regions(&regions);
            if !stricter_abi_and_runtime_constraints {
                assert_eq!(serialized.as_slice(), serialized_regions.as_slice());
            }
            let (de_program_id, de_accounts, de_instruction_data) = unsafe {
                deserialize(
                    if !stricter_abi_and_runtime_constraints {
                        serialized.as_slice_mut()
                    } else {
                        serialized_regions.as_slice_mut()
                    }
                    .first_mut()
                    .unwrap() as *mut u8,
                )
            };

            assert_eq!(&program_id, de_program_id);
            assert_eq!(instruction_data, de_instruction_data);
            assert_eq!(
                (de_instruction_data.first().unwrap() as *const u8).align_offset(BPF_ALIGN_OF_U128),
                0
            );
            for account_info in de_accounts {
                let index_in_transaction = invoke_context
                    .transaction_context
                    .find_index_of_account(account_info.key)
                    .unwrap();
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction)
                    .unwrap();
                assert_eq!(account.lamports(), account_info.lamports());
                assert_eq!(account.data(), &account_info.data.borrow()[..]);
                assert_eq!(account.owner(), account_info.owner);
                assert_eq!(account.executable(), account_info.executable);
                #[allow(deprecated)]
                {
                    // Using the sdk entrypoint, the rent-epoch is skipped
                    assert_eq!(0, account_info._unused);
                }

                assert_eq!(
                    (*account_info.lamports.borrow() as *const u64).align_offset(BPF_ALIGN_OF_U128),
                    0
                );
                assert_eq!(
                    account_info
                        .data
                        .borrow()
                        .as_ptr()
                        .align_offset(BPF_ALIGN_OF_U128),
                    0
                );
            }

            deserialize_parameters(
                &instruction_context,
                stricter_abi_and_runtime_constraints,
                false, // account_data_direct_mapping
                serialized.as_slice(),
                &accounts_metadata,
            )
            .unwrap();
            for (index_in_transaction, (_key, original_account)) in
                original_accounts.iter().enumerate()
            {
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction as IndexOfAccount)
                    .unwrap();
                assert_eq!(&*account, original_account);
            }

            // check serialize_parameters_unaligned
            invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(
                    7,
                    instruction_accounts,
                    instruction_data.clone(),
                )
                .unwrap();
            invoke_context.push().unwrap();
            let instruction_context = invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();

            let (mut serialized, regions, account_lengths, _instruction_data_offset) =
                serialize_parameters(
                    &instruction_context,
                    stricter_abi_and_runtime_constraints,
                    false, // account_data_direct_mapping
                    true,  // mask_out_rent_epoch_in_vm_serialization
                )
                .unwrap();
            let mut serialized_regions = concat_regions(&regions);

            let (de_program_id, de_accounts, de_instruction_data) = unsafe {
                deserialize_unaligned(
                    if !stricter_abi_and_runtime_constraints {
                        serialized.as_slice_mut()
                    } else {
                        serialized_regions.as_slice_mut()
                    }
                    .first_mut()
                    .unwrap() as *mut u8,
                )
            };
            assert_eq!(&program_id, de_program_id);
            assert_eq!(instruction_data, de_instruction_data);
            for account_info in de_accounts {
                let index_in_transaction = invoke_context
                    .transaction_context
                    .find_index_of_account(account_info.key)
                    .unwrap();
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction)
                    .unwrap();
                assert_eq!(account.lamports(), account_info.lamports());
                assert_eq!(account.data(), &account_info.data.borrow()[..]);
                assert_eq!(account.owner(), account_info.owner);
                assert_eq!(account.executable(), account_info.executable);
                #[allow(deprecated)]
                {
                    assert_eq!(u64::MAX, account_info._unused);
                }
            }

            deserialize_parameters(
                &instruction_context,
                stricter_abi_and_runtime_constraints,
                false, // account_data_direct_mapping
                serialized.as_slice(),
                &account_lengths,
            )
            .unwrap();
            for (index_in_transaction, (_key, original_account)) in
                original_accounts.iter().enumerate()
            {
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction as IndexOfAccount)
                    .unwrap();
                assert_eq!(&*account, original_account);
            }
        }
    }

    #[test]
    fn test_serialize_parameters_mask_out_rent_epoch_in_vm_serialization() {
        for mask_out_rent_epoch_in_vm_serialization in [false, true] {
            let transaction_accounts = vec![
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 1,
                        data: vec![1u8, 2, 3, 4, 5],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 2,
                        data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 200,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 3,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 300,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 4,
                        data: vec![1u8, 2, 3, 4, 5],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 5,
                        data: vec![11u8, 12, 13, 14, 15, 16, 17, 18, 19],
                        owner: bpf_loader::id(),
                        executable: true,
                        rent_epoch: 200,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 6,
                        data: vec![],
                        owner: bpf_loader::id(),
                        executable: false,
                        rent_epoch: 3100,
                    }),
                ),
                (
                    solana_pubkey::new_rand(),
                    AccountSharedData::from(Account {
                        lamports: 0,
                        data: vec![],
                        owner: bpf_loader_deprecated::id(),
                        executable: true,
                        rent_epoch: 0,
                    }),
                ),
            ];
            let instruction_accounts =
                deduplicated_instruction_accounts(&[1, 1, 2, 3, 4, 4, 5, 6], |index| index >= 4);
            with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
            invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(0, instruction_accounts.clone(), vec![])
                .unwrap();
            invoke_context.push().unwrap();
            let instruction_context = invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();

            // check serialize_parameters_aligned
            let (_serialized, regions, _accounts_metadata, _instruction_data_offset) =
                serialize_parameters(
                    &instruction_context,
                    true,
                    false, // account_data_direct_mapping
                    mask_out_rent_epoch_in_vm_serialization,
                )
                .unwrap();

            let mut serialized_regions = concat_regions(&regions);
            let (_de_program_id, de_accounts, _de_instruction_data) = unsafe {
                deserialize(serialized_regions.as_slice_mut().first_mut().unwrap() as *mut u8)
            };

            for account_info in de_accounts {
                // Using program-entrypoint, the rent-epoch will always be 0
                #[allow(deprecated)]
                {
                    assert_eq!(0, account_info._unused);
                }
            }

            // check serialize_parameters_unaligned
            invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(7, instruction_accounts, vec![])
                .unwrap();
            invoke_context.push().unwrap();
            let instruction_context = invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();

            let (_serialized, regions, _account_lengths, _instruction_data_offset) =
                serialize_parameters(
                    &instruction_context,
                    true,
                    false, // account_data_direct_mapping
                    mask_out_rent_epoch_in_vm_serialization,
                )
                .unwrap();
            let mut serialized_regions = concat_regions(&regions);

            let (_de_program_id, de_accounts, _de_instruction_data) = unsafe {
                deserialize_unaligned(
                    serialized_regions.as_slice_mut().first_mut().unwrap() as *mut u8
                )
            };
            for account_info in de_accounts {
                let index_in_transaction = invoke_context
                    .transaction_context
                    .find_index_of_account(account_info.key)
                    .unwrap();
                let account = invoke_context
                    .transaction_context
                    .accounts()
                    .try_borrow(index_in_transaction)
                    .unwrap();
                let expected_rent_epoch = if mask_out_rent_epoch_in_vm_serialization {
                    u64::MAX
                } else {
                    account.rent_epoch()
                };
                #[allow(deprecated)]
                {
                    assert_eq!(expected_rent_epoch, account_info._unused);
                }
            }
        }
    }

    // the old bpf_loader in-program deserializer bpf_loader::id()
    #[deny(unsafe_op_in_unsafe_fn)]
    unsafe fn deserialize_unaligned<'a>(
        input: *mut u8,
    ) -> (&'a Pubkey, Vec<AccountInfo<'a>>, &'a [u8]) {
        // this boring boilerplate struct is needed until inline const...
        struct Ptr<T>(std::marker::PhantomData<T>);
        impl<T> Ptr<T> {
            const COULD_BE_UNALIGNED: bool = std::mem::align_of::<T>() > 1;

            #[inline(always)]
            fn read_possibly_unaligned(input: *mut u8, offset: usize) -> T {
                unsafe {
                    let src = input.add(offset) as *const T;
                    if Self::COULD_BE_UNALIGNED {
                        src.read_unaligned()
                    } else {
                        src.read()
                    }
                }
            }

            // rustc inserts debug_assert! for misaligned pointer dereferences when
            // deserializing, starting from [1]. so, use std::mem::transmute as the last resort
            // while preventing clippy from complaining to suggest not to use it.
            // [1]: https://github.com/rust-lang/rust/commit/22a7a19f9333bc1fcba97ce444a3515cb5fb33e6
            // as for the ub nature of the misaligned pointer dereference, this is
            // acceptable in this code, given that this is cfg(test) and it's cared only with
            // x86-64 and the target only incurs some performance penalty, not like segfaults
            // in other targets.
            #[inline(always)]
            fn ref_possibly_unaligned<'a>(input: *mut u8, offset: usize) -> &'a T {
                #[allow(clippy::transmute_ptr_to_ref)]
                unsafe {
                    transmute(input.add(offset) as *const T)
                }
            }

            // See ref_possibly_unaligned's comment
            #[inline(always)]
            fn mut_possibly_unaligned<'a>(input: *mut u8, offset: usize) -> &'a mut T {
                #[allow(clippy::transmute_ptr_to_ref)]
                unsafe {
                    transmute(input.add(offset) as *mut T)
                }
            }
        }

        let mut offset: usize = 0;

        // number of accounts present

        let num_accounts = Ptr::<u64>::read_possibly_unaligned(input, offset) as usize;
        offset += size_of::<u64>();

        // account Infos

        let mut accounts = Vec::with_capacity(num_accounts);
        for _ in 0..num_accounts {
            let dup_info = Ptr::<u8>::read_possibly_unaligned(input, offset);
            offset += size_of::<u8>();
            if dup_info == NON_DUP_MARKER {
                let is_signer = Ptr::<u8>::read_possibly_unaligned(input, offset) != 0;
                offset += size_of::<u8>();

                let is_writable = Ptr::<u8>::read_possibly_unaligned(input, offset) != 0;
                offset += size_of::<u8>();

                let key = Ptr::<Pubkey>::ref_possibly_unaligned(input, offset);
                offset += size_of::<Pubkey>();

                let lamports = Rc::new(RefCell::new(Ptr::mut_possibly_unaligned(input, offset)));
                offset += size_of::<u64>();

                let data_len = Ptr::<u64>::read_possibly_unaligned(input, offset) as usize;
                offset += size_of::<u64>();

                let data = Rc::new(RefCell::new(unsafe {
                    from_raw_parts_mut(input.add(offset), data_len)
                }));
                offset += data_len;

                let owner: &Pubkey = Ptr::<Pubkey>::ref_possibly_unaligned(input, offset);
                offset += size_of::<Pubkey>();

                let executable = Ptr::<u8>::read_possibly_unaligned(input, offset) != 0;
                offset += size_of::<u8>();

                let unused = Ptr::<u64>::read_possibly_unaligned(input, offset);
                offset += size_of::<u64>();

                #[allow(deprecated)]
                accounts.push(AccountInfo {
                    key,
                    is_signer,
                    is_writable,
                    lamports,
                    data,
                    owner,
                    executable,
                    _unused: unused,
                });
            } else {
                // duplicate account, clone the original
                accounts.push(accounts.get(dup_info as usize).unwrap().clone());
            }
        }

        // instruction data

        let instruction_data_len = Ptr::<u64>::read_possibly_unaligned(input, offset) as usize;
        offset += size_of::<u64>();

        let instruction_data = unsafe { from_raw_parts(input.add(offset), instruction_data_len) };
        offset += instruction_data_len;

        // program Id

        let program_id = Ptr::<Pubkey>::ref_possibly_unaligned(input, offset);

        (program_id, accounts, instruction_data)
    }

    fn concat_regions(regions: &[MemoryRegion]) -> AlignedMemory<HOST_ALIGN> {
        let last_region = regions.last().unwrap();
        let mut mem = AlignedMemory::zero_filled(
            (last_region.vm_addr - MM_INPUT_START + last_region.len) as usize,
        );
        for region in regions {
            let host_slice = unsafe {
                slice::from_raw_parts(region.host_addr as *const u8, region.len as usize)
            };
            mem.as_slice_mut()[(region.vm_addr - MM_INPUT_START) as usize..][..region.len as usize]
                .copy_from_slice(host_slice)
        }
        mem
    }

    #[test]
    fn test_access_violation_handler() {
        let program_id = Pubkey::new_unique();
        let shared_account = AccountSharedData::new(0, 4, &program_id);
        let mut transaction_context = TransactionContext::new(
            vec![
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 4, &program_id),
                ), // readonly
                (Pubkey::new_unique(), shared_account.clone()), // writable shared
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 0, &program_id),
                ), // another writable account
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(
                        0,
                        MAX_PERMITTED_DATA_LENGTH as usize - 0x100,
                        &program_id,
                    ),
                ), // almost max sized writable account
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 0, &program_id),
                ), // writable dummy to burn accounts_resize_delta
                (
                    Pubkey::new_unique(),
                    AccountSharedData::new(0, 0x3000, &program_id),
                ), // writable dummy to burn accounts_resize_delta
                (program_id, AccountSharedData::default()),     // program
            ],
            Rent::default(),
            /* max_instruction_stack_depth */ 1,
            /* max_instruction_trace_length */ 1,
        );
        let transaction_accounts_indexes = [0, 1, 2, 3, 4, 5];
        let instruction_accounts =
            deduplicated_instruction_accounts(&transaction_accounts_indexes, |index| index > 0);
        transaction_context
            .configure_next_instruction_for_tests(6, instruction_accounts, vec![])
            .unwrap();
        transaction_context.push().unwrap();
        let instruction_context = transaction_context
            .get_current_instruction_context()
            .unwrap();
        let account_start_offsets = [
            MM_INPUT_START,
            MM_INPUT_START + 4 + MAX_PERMITTED_DATA_INCREASE as u64,
            MM_INPUT_START + (4 + MAX_PERMITTED_DATA_INCREASE as u64) * 2,
            MM_INPUT_START + (4 + MAX_PERMITTED_DATA_INCREASE as u64) * 3,
        ];
        let regions = account_start_offsets
            .iter()
            .enumerate()
            .map(|(index_in_instruction, account_start_offset)| {
                create_memory_region_of_account(
                    &mut instruction_context
                        .try_borrow_instruction_account(index_in_instruction as IndexOfAccount)
                        .unwrap(),
                    *account_start_offset,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let mut memory_mapping = MemoryMapping::new_with_access_violation_handler(
            regions,
            &config,
            SBPFVersion::V3,
            transaction_context.access_violation_handler(true, true),
        )
        .unwrap();

        // Reading readonly account is allowed
        memory_mapping
            .load::<u32>(account_start_offsets[0])
            .unwrap();

        // Reading writable account is allowed
        memory_mapping
            .load::<u32>(account_start_offsets[1])
            .unwrap();

        // Reading beyond readonly accounts current size is denied
        memory_mapping
            .load::<u32>(account_start_offsets[0] + 4)
            .unwrap_err();

        // Writing to readonly account is denied
        memory_mapping
            .store::<u32>(0, account_start_offsets[0])
            .unwrap_err();

        // Writing to shared writable account makes it unique (CoW logic)
        assert!(transaction_context
            .accounts()
            .try_borrow_mut(1)
            .unwrap()
            .is_shared());
        memory_mapping
            .store::<u32>(0, account_start_offsets[1])
            .unwrap();
        assert!(!transaction_context
            .accounts()
            .try_borrow_mut(1)
            .unwrap()
            .is_shared());
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .data()
                .len(),
            4,
        );

        // Reading beyond writable accounts current size grows is denied
        memory_mapping
            .load::<u32>(account_start_offsets[1] + 4)
            .unwrap_err();

        // Writing beyond writable accounts current size grows it
        // to original length plus MAX_PERMITTED_DATA_INCREASE
        memory_mapping
            .store::<u32>(0, account_start_offsets[1] + 4)
            .unwrap();
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .data()
                .len(),
            4 + MAX_PERMITTED_DATA_INCREASE,
        );
        assert!(
            transaction_context
                .accounts()
                .try_borrow(1)
                .unwrap()
                .data()
                .len()
                < 0x3000
        );

        // Writing beyond almost max sized writable accounts current size only grows it
        // to MAX_PERMITTED_DATA_LENGTH
        memory_mapping
            .store::<u32>(0, account_start_offsets[3] + MAX_PERMITTED_DATA_LENGTH - 4)
            .unwrap();
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(3)
                .unwrap()
                .data()
                .len(),
            MAX_PERMITTED_DATA_LENGTH as usize,
        );

        // Accessing the rest of the address space reserved for
        // the almost max sized writable account is denied
        memory_mapping
            .load::<u32>(account_start_offsets[3] + MAX_PERMITTED_DATA_LENGTH)
            .unwrap_err();
        memory_mapping
            .store::<u32>(0, account_start_offsets[3] + MAX_PERMITTED_DATA_LENGTH)
            .unwrap_err();

        // Burn through most of the accounts_resize_delta budget
        let remaining_allowed_growth: usize = 0x700;
        for index_in_instruction in 4..6 {
            let mut borrowed_account = instruction_context
                .try_borrow_instruction_account(index_in_instruction)
                .unwrap();
            borrowed_account
                .set_data_from_slice(&vec![0u8; MAX_PERMITTED_DATA_LENGTH as usize])
                .unwrap();
        }
        assert_eq!(
            transaction_context.accounts().resize_delta(),
            MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION
                - remaining_allowed_growth as i64,
        );

        // Writing beyond empty writable accounts current size
        // only grows it to fill up MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION
        memory_mapping
            .store::<u32>(0, account_start_offsets[2] + 0x500)
            .unwrap();
        assert_eq!(
            transaction_context
                .accounts()
                .try_borrow(2)
                .unwrap()
                .data()
                .len(),
            remaining_allowed_growth,
        );
    }
}
