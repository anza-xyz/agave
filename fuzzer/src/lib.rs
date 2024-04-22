#![allow(clippy::missing_safety_doc)]

use {
    lazy_static::lazy_static,
    prost::Message,
    solana_program_runtime::{
        compute_budget::ComputeBudget,
        invoke_context::InvokeContext,
        loaded_programs::{ProgramCacheEntry, ProgramCacheForTxBatch},
        sysvar_cache::SysvarCache,
        timings::ExecuteTimings,
    },
    solana_sdk::{
        account::{Account, AccountSharedData, ReadableAccount},
        feature_set::*,
        instruction::{AccountMeta, InstructionError},
        pubkey::Pubkey,
        stable_layout::stable_instruction::StableInstruction,
        sysvar::rent::Rent,
        transaction_context::{
            IndexOfAccount, InstructionAccount, TransactionAccount, TransactionContext,
        },
    },
    std::{collections::HashMap, ffi::c_int, sync::Arc},
    thiserror::Error,
};

// macro to rewrite &[IDENTIFIER, ...] to &[feature_u64(IDENTIFIER::id()), ...]
macro_rules! feature_list {
    ($($feature:ident),*$(,)?) => {
        &[$(feature_u64(&$feature::id())),*]
    };
}

static HARDCODED_FEATURES: &[u64] = feature_list![
    secp256k1_program_enabled,
    system_transfer_zero_check,
    native_programs_consume_cu,
    dedupe_config_program_signers,
    vote_stake_checked_instructions,
    require_custodian_for_locked_stake_authorize,
    stake_merge_with_unmatched_credits_observed,
    require_rent_exempt_split_destination,
    vote_authorize_with_seed,
    allow_votes_to_directly_update_vote_state,
    compact_vote_state_updates,
];

static SUPPORTED_FEATURES: &[u64] = feature_list![
    // Active on all clusters, but not cleaned up.
    pico_inflation,
    warp_timestamp_again,
    disable_fees_sysvar,
    disable_deploy_of_alloc_free_syscall,
    set_exempt_rent_epoch_max,
    incremental_snapshot_only_incremental_hash_calculation,
    relax_authority_signer_check_for_lookup_table_creation,
    commission_updates_only_allowed_in_first_half_of_epoch,
    enable_turbine_fanout_experiments,
    update_hashes_per_tick,
    reduce_stake_warmup_cooldown,
    enable_early_verification_of_account_modifications,
    // Active on testnet & devnet.
    libsecp256k1_fail_on_bad_count2,
    enable_bpf_loader_set_authority_checked_ix,
    enable_alt_bn128_syscall,
    switch_to_new_elf_parser,
    vote_state_add_vote_latency,
    update_hashes_per_tick2,
    update_hashes_per_tick3,
    update_hashes_per_tick4,
    update_hashes_per_tick5,
    validate_fee_collector_account,
    // Active on testnet.
    stake_raise_minimum_delegation_to_1_sol,
    update_hashes_per_tick6,
    // Active on devnet.
    blake3_syscall_enabled,
    curve25519_syscall_enabled,
    libsecp256k1_fail_on_bad_count,
    reject_callx_r10,
    increase_tx_account_lock_limit,
    // Inactive on all clusters.
    zk_token_sdk_enabled,
    enable_partitioned_epoch_reward,
    stake_minimum_delegation_for_rewards,
    stake_redelegate_instruction,
    skip_rent_rewrites,
    loosen_cpi_size_restriction,
    disable_turbine_fanout_experiments,
    enable_big_mod_exp_syscall,
    apply_cost_tracker_during_replay,
    include_loaded_accounts_data_size_in_fee_calculation,
    bpf_account_data_direct_mapping,
    last_restart_slot_sysvar,
    enable_poseidon_syscall,
    timely_vote_credits,
    remaining_compute_units_syscall_enabled,
    enable_program_runtime_v2_and_loader_v4,
    enable_alt_bn128_compression_syscall,
    disable_rent_fees_collection,
    enable_zk_transfer_with_fee,
    drop_legacy_shreds,
    allow_commission_decrease_at_any_time,
    consume_blockstore_duplicate_proofs,
    index_erasure_conflict_duplicate_proofs,
    merkle_conflict_duplicate_proofs,
    enable_zk_proof_from_account,
    curve25519_restrict_msm_length,
    cost_model_requested_write_lock_cost,
    enable_gossip_duplicate_proof_ingestion,
    enable_chained_merkle_shreds,
    // These two were force-activated, but the gate remains on the BPF Loader.
    disable_bpf_loader_instructions,
];

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/org.solana.sealevel.v1.rs"));
}

lazy_static! {
    static ref INDEXED_FEATURES: HashMap<u64, Pubkey> = {
        FEATURE_NAMES
            .iter()
            .map(|(pubkey, _)| (feature_u64(pubkey), *pubkey))
            .collect()
    };
}

impl From<&proto::FeatureSet> for FeatureSet {
    fn from(input: &proto::FeatureSet) -> Self {
        let mut feature_set = FeatureSet::default();
        for id in &input.features {
            if let Some(pubkey) = INDEXED_FEATURES.get(id) {
                feature_set.activate(pubkey, 0);
            }
        }
        feature_set
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum Error {
    #[error("Invalid protobuf")]
    InvalidProtobuf(#[from] prost::DecodeError),

    #[error("Integer out of range")]
    IntegerOutOfRange,

    #[error("Invalid hash bytes")]
    InvalidHashBytes,

    #[error("Invalid public key bytes")]
    InvalidPubkeyBytes,

    #[error("Account missing")]
    AccountMissing,

    #[error("Invalid fixture input")]
    InvalidFixtureInput,

    #[error("Invalid fixture output")]
    InvalidFixtureOutput,
}

pub struct InstrContext {
    pub loader_id: Option<Pubkey>,
    pub feature_set: FeatureSet,
    pub accounts: Vec<(Pubkey, Account)>,
    pub instruction: StableInstruction,
    pub cu_avail: u64,
}

impl TryFrom<proto::InstrContext> for InstrContext {
    type Error = Error;

    fn try_from(input: proto::InstrContext) -> Result<Self, Self::Error> {
        let program_id = Pubkey::new_from_array(
            input
                .program_id
                .try_into()
                .map_err(|_| Error::InvalidPubkeyBytes)?,
        );

        let loader_id = if input.loader_id.is_empty() {
            None
        } else {
            Some(Pubkey::new_from_array(
                input
                    .loader_id
                    .try_into()
                    .map_err(|_| Error::InvalidPubkeyBytes)?,
            ))
        };

        let feature_set: FeatureSet = input
            .epoch_context
            .as_ref()
            .and_then(|epoch_ctx| epoch_ctx.features.as_ref())
            .map(|fs| fs.into())
            .unwrap_or_default();

        let accounts: Vec<(Pubkey, Account)> = input
            .accounts
            .into_iter()
            .map(|acct_state| acct_state.try_into())
            .collect::<Result<Vec<_>, _>>()?;

        let instruction_accounts = input
            .instr_accounts
            .into_iter()
            .map(|acct| {
                if acct.index as usize >= accounts.len() {
                    return Err(Error::AccountMissing);
                }
                Ok(AccountMeta {
                    pubkey: accounts[acct.index as usize].0,
                    is_signer: acct.is_signer,
                    is_writable: acct.is_writable,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let instruction = StableInstruction {
            accounts: instruction_accounts.into(),
            data: input.data.into(),
            program_id,
        };

        Ok(Self {
            loader_id,
            feature_set,
            accounts,
            instruction,
            cu_avail: input.cu_avail,
        })
    }
}

fn instr_err_to_num(error: &InstructionError) -> i32 {
    let serialized_err = bincode::serialize(error).unwrap();
    i32::from_le_bytes((&serialized_err[0..4]).try_into().unwrap())
}

pub struct InstrEffects {
    pub result: Option<InstructionError>,
    pub custom_err: Option<u32>,
    pub modified_accounts: Vec<(Pubkey, Account)>,
    pub cu_avail: u64,
}

impl From<InstrEffects> for proto::InstrEffects {
    fn from(val: InstrEffects) -> Self {
        proto::InstrEffects {
            result: val
                .result
                .as_ref()
                .map(instr_err_to_num)
                .unwrap_or_default(),
            custom_err: val.custom_err.unwrap_or_default(),
            modified_accounts: val
                .modified_accounts
                .into_iter()
                .map(|(pubkey, account)| proto::AcctState {
                    address: pubkey.to_bytes().to_vec(),
                    owner: account.owner.to_bytes().to_vec(),
                    lamports: account.lamports,
                    data: account.data.to_vec(),
                    executable: account.executable,
                    rent_epoch: account.rent_epoch,
                })
                .collect(),
            cu_avail: val.cu_avail,
        }
    }
}

pub fn execute_instr_proto(input: proto::InstrContext) -> Option<proto::InstrEffects> {
    let Ok(instr_context) = InstrContext::try_from(input) else {
        return None;
    };
    let instr_effects = execute_instr(instr_context);
    instr_effects.map(Into::into)
}

fn load_builtins(cache: &mut ProgramCacheForTxBatch) {
    cache.replenish(
        solana_address_lookup_table_program::id(),
        Arc::new(ProgramCacheEntry::new_builtin(
            0u64,
            0usize,
            solana_address_lookup_table_program::processor::Entrypoint::vm,
        )),
    );
    cache.replenish(
        solana_config_program::id(),
        Arc::new(ProgramCacheEntry::new_builtin(
            0u64,
            0usize,
            solana_config_program::config_processor::Entrypoint::vm,
        )),
    );
    cache.replenish(
        solana_stake_program::id(),
        Arc::new(ProgramCacheEntry::new_builtin(
            0u64,
            0usize,
            solana_stake_program::stake_instruction::Entrypoint::vm,
        )),
    );
    cache.replenish(
        solana_system_program::id(),
        Arc::new(ProgramCacheEntry::new_builtin(
            0u64,
            0usize,
            solana_system_program::system_processor::Entrypoint::vm,
        )),
    );
    cache.replenish(
        solana_vote_program::id(),
        Arc::new(ProgramCacheEntry::new_builtin(
            0u64,
            0usize,
            solana_vote_program::vote_processor::Entrypoint::vm,
        )),
    );
}

fn execute_instr(input: InstrContext) -> Option<InstrEffects> {
    // TODO this shouldn't be default
    let compute_budget = ComputeBudget {
        compute_unit_limit: input.cu_avail,
        ..ComputeBudget::default()
    };

    let mut sysvar_cache = SysvarCache::default();

    sysvar_cache.fill_missing_entries(|pubkey, callbackback| {
        if let Some(account) = input.accounts.iter().find(|(key, _)| key == pubkey) {
            if account.1.lamports > 0 {
                callbackback(&account.1.data);
            }
        }
    });

    // Add checks for rent boundaries
    let rent: Rent;
    if let Ok(rent_) = sysvar_cache.get_rent() {
        if rent_.lamports_per_byte_year > u32::MAX.into()
            || rent_.exemption_threshold > 999.0
            || rent_.exemption_threshold < 0.0
            || rent_.burn_percent > 100
        {
            return None;
        }
        rent = (*rent_).clone();
    } else {
        rent = Rent::default();
    }

    let mut transaction_accounts =
        Vec::<TransactionAccount>::with_capacity(input.accounts.len().saturating_add(1));
    #[allow(deprecated)]
    input
        .accounts
        .into_iter()
        .map(|(pubkey, account)| (pubkey, AccountSharedData::from(account)))
        .for_each(|x| transaction_accounts.push(x));

    let program_idx = if let Some(index) = transaction_accounts
        .iter()
        .position(|(pubkey, _)| *pubkey == input.instruction.program_id)
    {
        index
    } else {
        transaction_accounts.push((
            input.instruction.program_id,
            AccountSharedData::from(Account {
                lamports: 10000000,
                data: b"Solana Program".to_vec(),
                owner: solana_sdk::native_loader::id(),
                executable: true,
                rent_epoch: 0,
            }),
        ));
        transaction_accounts.len().saturating_sub(1)
    };

    // Skip if the program account is not owned by the native loader
    // (Would call the owner instead)
    if transaction_accounts[program_idx].1.owner() != &solana_sdk::native_loader::id() {
        return None;
    }

    let mut transaction_context = TransactionContext::new(
        transaction_accounts.clone(),
        rent,
        compute_budget.max_invoke_stack_height,
        compute_budget.max_instruction_trace_length,
    );

    // sigh ... What is this mess?
    let mut programs_loaded_for_tx_batch = ProgramCacheForTxBatch::default();
    load_builtins(&mut programs_loaded_for_tx_batch);

    let mut programs_modified_by_tx = ProgramCacheForTxBatch::default();

    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();

    let mut invoke_context = InvokeContext::new(
        &mut transaction_context,
        &sysvar_cache,
        None,
        compute_budget,
        &programs_loaded_for_tx_batch,
        &mut programs_modified_by_tx,
        Arc::new(input.feature_set),
        blockhash,
        lamports_per_signature,
    );

    let program_indices = &[program_idx as u16];

    let mut compute_units_consumed = 0u64;

    let mut timings = ExecuteTimings::default();

    let mut instruction_accounts: Vec<InstructionAccount> =
        Vec::with_capacity(input.instruction.accounts.len());
    for (instruction_account_index, account_meta) in
        input.instruction.accounts.as_ref().iter().enumerate()
    {
        let index_in_transaction = transaction_accounts
            .iter()
            .position(|(key, _account)| *key == account_meta.pubkey)
            .unwrap_or(transaction_accounts.len())
            as IndexOfAccount;
        let index_in_callee = instruction_accounts
            .get(0..instruction_account_index)
            .unwrap()
            .iter()
            .position(|instruction_account| {
                instruction_account.index_in_transaction == index_in_transaction
            })
            .unwrap_or(instruction_account_index) as IndexOfAccount;
        instruction_accounts.push(InstructionAccount {
            index_in_transaction,
            index_in_caller: index_in_transaction,
            index_in_callee,
            is_signer: account_meta.is_signer,
            is_writable: account_meta.is_writable,
        });
    }

    // The Agave impl of the stake program panics if there is no epoch schedule
    if transaction_accounts[program_idx].0 == solana_stake_program::id()
        && sysvar_cache.get_epoch_schedule().is_err()
    {
        return None;
    }

    let result = invoke_context.process_instruction(
        &input.instruction.data,
        &instruction_accounts,
        program_indices,
        &mut compute_units_consumed,
        &mut timings,
    );

    Some(InstrEffects {
        custom_err: if let Err(InstructionError::Custom(x)) = result {
            Some(x)
        } else {
            None
        },
        result: result.err(),
        modified_accounts: transaction_context
            .deconstruct_without_keys()
            .unwrap()
            .into_iter()
            .take(transaction_accounts.len().saturating_sub(1))
            .enumerate()
            .map(|(index, data)| (transaction_accounts[index].0, data.into()))
            .collect(),
        cu_avail: input.cu_avail.saturating_sub(compute_units_consumed),
    })
}

impl TryFrom<proto::AcctState> for (Pubkey, Account) {
    type Error = Error;

    fn try_from(input: proto::AcctState) -> Result<Self, Self::Error> {
        let pubkey = Pubkey::new_from_array(
            input
                .address
                .try_into()
                .map_err(|_| Error::InvalidPubkeyBytes)?,
        );
        let owner = Pubkey::new_from_array(
            input
                .owner
                .try_into()
                .map_err(|_| Error::InvalidPubkeyBytes)?,
        );

        Ok((
            pubkey,
            Account {
                lamports: input.lamports,
                data: input.data,
                owner,
                executable: input.executable,
                rent_epoch: input.rent_epoch,
            },
        ))
    }
}

#[no_mangle]
pub unsafe extern "C" fn sol_compat_init() {}

#[repr(C)]
pub struct SolCompatFeatures {
    pub struct_size: u64,
    pub hardcoded_features: *const u64,
    pub hardcoded_features_len: u64,
    pub supported_features: *const u64,
    pub supported_features_len: u64,
}

unsafe impl Send for SolCompatFeatures {}
unsafe impl Sync for SolCompatFeatures {}

pub const fn feature_u64(feature: &Pubkey) -> u64 {
    let feature_id = feature.to_bytes();
    feature_id[0] as u64
        | (feature_id[1] as u64) << 8
        | (feature_id[2] as u64) << 16
        | (feature_id[3] as u64) << 24
        | (feature_id[4] as u64) << 32
        | (feature_id[5] as u64) << 40
        | (feature_id[6] as u64) << 48
        | (feature_id[7] as u64) << 56
}

static FEATURES: SolCompatFeatures = SolCompatFeatures {
    struct_size: std::mem::size_of::<SolCompatFeatures>() as u64,
    hardcoded_features: HARDCODED_FEATURES.as_ptr(),
    hardcoded_features_len: HARDCODED_FEATURES.len() as u64,
    supported_features: SUPPORTED_FEATURES.as_ptr(),
    supported_features_len: SUPPORTED_FEATURES.len() as u64,
};

#[no_mangle]
pub unsafe extern "C" fn sol_compat_get_features_v1() -> *const SolCompatFeatures {
    &FEATURES
}

#[no_mangle]
pub unsafe extern "C" fn sol_compat_fini() {}

#[no_mangle]
pub unsafe extern "C" fn sol_compat_instr_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = std::slice::from_raw_parts(in_ptr, in_sz as usize);
    let Ok(instr_context) = proto::InstrContext::decode(in_slice) else {
        return 0;
    };
    let Some(instr_effects) = execute_instr_proto(instr_context) else {
        return 0;
    };
    let out_slice = std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize);
    let out_vec = instr_effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    *out_psz = out_vec.len() as u64;

    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_program_exec() {
        // Ensure that a basic account transfer works
        let input = proto::InstrContext {
            program_id: vec![0u8; 32],
            loader_id: Pubkey::default().to_bytes().to_vec(),
            accounts: vec![
                proto::AcctState {
                    address: vec![1u8; 32],
                    owner: vec![0u8; 32],
                    lamports: 1000,
                    data: vec![],
                    executable: false,
                    rent_epoch: 0,
                },
                proto::AcctState {
                    address: vec![2u8; 32],
                    owner: vec![0u8; 32],
                    lamports: 0,
                    data: vec![],
                    executable: false,
                    rent_epoch: 0,
                },
            ],
            instr_accounts: vec![
                proto::InstrAcct {
                    index: 0,
                    is_signer: true,
                    is_writable: true,
                },
                proto::InstrAcct {
                    index: 1,
                    is_signer: false,
                    is_writable: true,
                },
            ],
            data: vec![
                // Transfer
                0x02, 0x00, 0x00, 0x00, // Lamports
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            cu_avail: 10000u64,
            epoch_context: None,
            slot_context: None,
            txn_context: None,
        };
        let output = execute_instr_proto(input);
        assert_eq!(
            output,
            Some(proto::InstrEffects {
                result: 0,
                custom_err: 0,
                modified_accounts: vec![
                    proto::AcctState {
                        address: vec![1u8; 32],
                        owner: vec![0u8; 32],
                        lamports: 999,
                        data: vec![],
                        executable: false,
                        rent_epoch: 0,
                    },
                    proto::AcctState {
                        address: vec![2u8; 32],
                        owner: vec![0u8; 32],
                        lamports: 1,
                        data: vec![],
                        executable: false,
                        rent_epoch: 0,
                    },
                ],
                cu_avail: 9850u64,
            })
        );
    }
}
