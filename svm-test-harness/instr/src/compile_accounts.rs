//! Instruction <-> Transaction account compilation, with key deduplication,
//! privilege handling, and program account stubbing.

use {
    solana_account::{Account, AccountSharedData},
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_transaction_context::{instruction_accounts::InstructionAccount, IndexOfAccount},
    std::collections::{BTreeMap, HashMap, HashSet},
};

#[derive(Debug, Default)]
struct KeyMap {
    map: BTreeMap<Pubkey, (bool, bool)>,
    program_ids: HashSet<Pubkey>,
}

impl KeyMap {
    fn add_account(&mut self, meta: &AccountMeta) {
        let entry = self.map.entry(meta.pubkey).or_default();
        entry.0 |= meta.is_signer;
        entry.1 |= meta.is_writable;
    }

    fn add_accounts<'a>(&mut self, accounts: impl Iterator<Item = &'a AccountMeta>) {
        for meta in accounts {
            self.add_account(meta);
        }
    }

    fn add_instruction(&mut self, instruction: &Instruction) {
        self.add_program(instruction.program_id);
        self.add_accounts(instruction.accounts.iter());
    }

    fn add_program(&mut self, program_id: Pubkey) {
        self.map.insert(program_id, (false, false));
        self.program_ids.insert(program_id);
    }

    fn compile_from_instruction(instruction: &Instruction) -> Self {
        let mut map = Self::default();
        map.add_instruction(instruction);
        map
    }

    fn is_signer_at_index(&self, index: usize) -> bool {
        self.map
            .values()
            .nth(index)
            .map(|(s, _)| *s)
            .unwrap_or(false)
    }

    fn is_writable_at_index(&self, index: usize) -> bool {
        self.map
            .values()
            .nth(index)
            .map(|(_, w)| *w)
            .unwrap_or(false)
    }

    fn keys(&self) -> impl Iterator<Item = &Pubkey> {
        self.map.keys()
    }

    fn position(&self, key: &Pubkey) -> Option<usize> {
        self.map.keys().position(|k| k == key)
    }
}

struct CompiledInstructionWithoutData {
    program_id_index: u8,
    accounts: Vec<u8>,
}

fn compile_instruction_without_data(
    key_map: &KeyMap,
    instruction: &Instruction,
) -> CompiledInstructionWithoutData {
    let program_id_index = key_map
        .position(&instruction.program_id)
        .expect("Program ID required by the instruction is not mapped");

    let program_id_index =
        u8::try_from(program_id_index).expect("Account index exceeds maximum of 255");

    let accounts: Vec<u8> = instruction
        .accounts
        .iter()
        .map(|account_meta| {
            let index = key_map
                .position(&account_meta.pubkey)
                .expect("An account required by the instruction was not provided");

            u8::try_from(index).expect("Account index exceeds maximum of 255")
        })
        .collect();

    CompiledInstructionWithoutData {
        program_id_index,
        accounts,
    }
}

fn compile_instruction_accounts(
    key_map: &KeyMap,
    compiled_instruction: &CompiledInstructionWithoutData,
) -> Vec<InstructionAccount> {
    compiled_instruction
        .accounts
        .iter()
        .map(|&index_in_transaction| {
            let index_in_transaction = index_in_transaction as usize;
            InstructionAccount::new(
                index_in_transaction as IndexOfAccount,
                key_map.is_signer_at_index(index_in_transaction),
                key_map.is_writable_at_index(index_in_transaction),
            )
        })
        .collect()
}

fn compile_transaction_accounts<'a>(
    key_map: &KeyMap,
    accounts: impl Iterator<Item = &'a (Pubkey, Account)>,
    fallback_accounts: &HashMap<Pubkey, Account>,
) -> Vec<(Pubkey, AccountSharedData)> {
    let accounts: Vec<_> = accounts.collect();
    key_map
        .keys()
        .map(|key| {
            let account = accounts
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, a)| AccountSharedData::from(a.clone()))
                .or_else(|| fallback_accounts.get(key).cloned().map(Into::into))
                .expect("An account required by the instruction was not provided");
            (*key, account)
        })
        .collect()
}

pub struct CompiledAccounts {
    pub program_id_index: u16,
    pub instruction_accounts: Vec<InstructionAccount>,
    pub transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
}

pub fn compile_accounts<'a>(
    instruction: &Instruction,
    accounts: impl Iterator<Item = &'a (Pubkey, Account)>,
    fallback_accounts: &HashMap<Pubkey, Account>,
) -> CompiledAccounts {
    let key_map = KeyMap::compile_from_instruction(instruction);
    let compiled_instruction = compile_instruction_without_data(&key_map, instruction);
    let instruction_accounts = compile_instruction_accounts(&key_map, &compiled_instruction);
    let transaction_accounts = compile_transaction_accounts(&key_map, accounts, fallback_accounts);

    CompiledAccounts {
        program_id_index: compiled_instruction.program_id_index as u16,
        instruction_accounts,
        transaction_accounts,
    }
}
