//! Retain slot deltas until PubSub has delivered their commitment notifications.

use {
    solana_account::AccountSharedData,
    solana_accounts_db::accounts::SlotAccounts,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, BankId, TransactionLogCollector, TransactionLogInfo},
        bank_forks::BankForks,
    },
    std::{
        collections::{BTreeMap, HashMap, VecDeque},
        sync::{Arc, RwLock, Weak},
    },
};

// Bound bank metadata as well as account data. A lagging subscriber must reconnect
// if the data it needs has already left this window.
pub(crate) const MAX_NOTIFICATION_BANKS: usize = 512;

pub(crate) struct NotificationBank {
    pub bank: Arc<Bank>,
    pub frozen: bool,
    logs: Arc<RwLock<TransactionLogCollector>>,
    accounts: SlotAccounts,
    bytes: usize,
}

impl NotificationBank {
    fn new(bank: Arc<Bank>) -> Self {
        let frozen = bank.is_frozen();
        let accounts = bank.accounts().snapshot_slot(bank.slot(), bank.bank_id());
        let account_bytes = accounts.size();
        let logs = if frozen {
            bank.transaction_log_collector.clone()
        } else {
            let collector = bank.transaction_log_collector.read().unwrap();
            Arc::new(RwLock::new(TransactionLogCollector {
                logs: collector.logs.clone(),
                mentioned_address_map: collector.mentioned_address_map.clone(),
            }))
        };
        let log_bytes = logs
            .read()
            .unwrap()
            .logs
            .iter()
            .map(|log| {
                std::mem::size_of_val(log)
                    + log
                        .log_messages
                        .iter()
                        .map(|s| s.capacity() + std::mem::size_of::<String>())
                        .sum::<usize>()
            })
            .sum::<usize>();
        Self {
            bank,
            frozen,
            logs,
            accounts,
            bytes: account_bytes + log_bytes,
        }
    }

    pub fn logs(&self, address: Option<&Pubkey>) -> Option<Vec<TransactionLogInfo>> {
        self.logs.read().unwrap().get_logs_for_address(address)
    }

    pub fn program_accounts(&self, program: &Pubkey) -> Vec<(Pubkey, AccountSharedData)> {
        self.accounts.load_by_program(program)
    }
}

pub(crate) struct NotificationBankRef {
    pub slot: Slot,
    pub parent_slot: Slot,
    pub bank_id: BankId,
    pub rooted: bool,
    pub bank: Weak<NotificationBank>,
}

impl From<&Arc<NotificationBank>> for NotificationBankRef {
    fn from(bank: &Arc<NotificationBank>) -> Self {
        Self {
            slot: bank.bank.slot(),
            parent_slot: bank.bank.parent_slot(),
            bank_id: bank.bank.bank_id(),
            rooted: false,
            bank: Arc::downgrade(bank),
        }
    }
}

#[derive(Default)]
pub struct NotificationBankChain {
    pub(crate) banks: Vec<NotificationBankRef>,
    pub(crate) slot: Slot,
}

pub(crate) struct NotificationBanks {
    banks: HashMap<u64, Arc<NotificationBank>>,
    frozen: HashMap<BankId, u64>,
    order: VecDeque<u64>,
    next_id: u64,
    roots: BTreeMap<Slot, NotificationBankRef>,
    bytes: usize,
    max_bytes: usize,
}

impl NotificationBanks {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            banks: HashMap::new(),
            frozen: HashMap::new(),
            next_id: 0,
            order: VecDeque::new(),
            roots: BTreeMap::new(),
            bytes: 0,
            max_bytes,
        }
    }

    fn retain(&mut self, bank: Arc<Bank>) -> NotificationBankRef {
        let bank_id = bank.bank_id();
        if let Some(id) = self.frozen.get(&bank_id) {
            return NotificationBankRef::from(&self.banks[id]);
        }
        let bank = Arc::new(NotificationBank::new(bank));
        let reference = NotificationBankRef::from(&bank);
        let id = self.next_id;
        self.next_id += 1;
        if bank.frozen {
            self.frozen.insert(bank_id, id);
        }
        self.banks.insert(id, bank.clone());
        self.bytes += bank.bytes;
        self.order.push_back(id);
        while self.bytes > self.max_bytes || self.order.len() > MAX_NOTIFICATION_BANKS {
            let id = self.order.pop_front().unwrap();
            let old = self.banks.remove(&id).unwrap();
            self.bytes -= old.bytes;
            if old.frozen {
                self.frozen.remove(&old.bank.bank_id());
            }
        }
        reference
    }

    // Must run before squash/clean can discard the newly rooted deltas.
    pub fn retain_roots(&mut self, banks: &[Arc<Bank>]) {
        let mut banks: Vec<_> = banks.iter().collect();
        banks.sort_unstable_by_key(|bank| bank.slot());
        for bank in banks {
            if self
                .roots
                .get(&bank.slot())
                .is_some_and(|root| root.bank_id == bank.bank_id())
            {
                continue;
            }
            let mut reference = self.retain(bank.clone());
            reference.rooted = true;
            self.roots.insert(bank.slot(), reference);
        }
        while self.roots.len() > MAX_NOTIFICATION_BANKS {
            self.roots.pop_first();
        }
    }

    pub fn chain(&mut self, slot: Slot, forks: &BankForks) -> NotificationBankChain {
        let Some(bank) = forks.get(slot).or_else(|| {
            self.roots
                .get(&slot)
                .and_then(|root| root.bank.upgrade())
                .map(|snapshot| snapshot.bank.clone())
        }) else {
            return NotificationBankChain {
                banks: Vec::new(),
                slot,
            };
        };
        let ancestors: std::collections::HashSet<_> = bank.ancestors.iter_bank_ids().collect();
        let mut chain = BTreeMap::new();
        for (&root, reference) in self.roots.range(..slot) {
            if root > forks.root() && !ancestors.contains(&reference.bank_id) {
                continue;
            }
            chain.insert(
                root,
                NotificationBankRef {
                    slot: root,
                    parent_slot: reference.parent_slot,
                    bank_id: reference.bank_id,
                    rooted: true,
                    bank: reference.bank.clone(),
                },
            );
        }
        let mut parents = bank.parents();
        parents.reverse();
        parents.push(bank);
        for bank in parents {
            // A retained root may already have been cleaned. Keep the original
            // snapshot instead of replacing it with an incomplete delta.
            let reference = if let Some(root) = self.roots.get(&bank.slot())
                && root.bank_id == bank.bank_id()
            {
                NotificationBankRef {
                    slot: root.slot,
                    parent_slot: root.parent_slot,
                    bank_id: root.bank_id,
                    rooted: true,
                    bank: root.bank.clone(),
                }
            } else {
                self.retain(bank.clone())
            };
            chain.insert(bank.slot(), reference);
        }
        NotificationBankChain {
            banks: chain.into_values().collect(),
            slot,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::ReadableAccount,
        solana_runtime::{bank::SlotLeader, genesis_utils::create_genesis_config},
    };

    #[test]
    fn test_root_retention_limit_and_duplicate_capture() {
        let genesis = create_genesis_config(1_000_000).genesis_config;
        let mut parent = Arc::new(Bank::new_for_tests(&genesis));
        let mut retained = NotificationBanks::new(usize::MAX);
        let mut first = None;
        for slot in 1..=MAX_NOTIFICATION_BANKS as u64 + 1 {
            let bank = Arc::new(Bank::new_from_parent(parent, SlotLeader::default(), slot));
            bank.freeze();
            retained.retain_roots(std::slice::from_ref(&bank));
            if first.is_none() {
                first = Some(retained.roots[&slot].bank.clone());
            }
            let bytes = retained.bytes;
            let next_id = retained.next_id;
            retained.retain_roots(std::slice::from_ref(&bank));
            assert_eq!(retained.bytes, bytes);
            assert_eq!(retained.next_id, next_id);
            parent = bank;
        }
        assert_eq!(retained.roots.len(), MAX_NOTIFICATION_BANKS);
        assert_eq!(retained.banks.len(), MAX_NOTIFICATION_BANKS);
        assert!(!retained.roots.contains_key(&1));
        assert!(first.unwrap().upgrade().is_none());
        assert!(
            retained.roots[&(MAX_NOTIFICATION_BANKS as u64 + 1)]
                .bank
                .upgrade()
                .is_some()
        );
    }

    #[test]
    fn test_chain_excludes_pending_roots_on_other_forks() {
        let genesis = create_genesis_config(1_000_000).genesis_config;
        let forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis));
        let root = forks.read().unwrap().root_bank();
        let other = Arc::new(Bank::new_from_parent(
            root.clone(),
            SlotLeader::default(),
            1,
        ));
        other.freeze();
        let tip = Bank::new_from_parent(root, SlotLeader::default(), 2);
        tip.freeze();
        let tip = forks.write().unwrap().insert(tip).clone_without_scheduler();
        let mut retained = NotificationBanks::new(usize::MAX);
        retained.retain_roots(std::slice::from_ref(&other));
        let chain = retained.chain(2, &forks.read().unwrap());
        assert_eq!(
            chain
                .banks
                .iter()
                .map(|reference| reference.slot)
                .collect::<Vec<_>>(),
            [0, 2]
        );
        assert_eq!(chain.banks.last().unwrap().bank_id, tip.bank_id());
        assert!(
            chain
                .banks
                .iter()
                .all(|reference| reference.bank_id != other.bank_id())
        );
    }

    #[test]
    fn test_rooted_deltas_survive_flush_and_clean() {
        for flush_before_capture in [false, true] {
            let genesis = create_genesis_config(1_000_000).genesis_config;
            let bank0 = Arc::new(Bank::new_for_tests(&genesis));
            let address = Pubkey::new_unique();
            let owner = Pubkey::new_unique();
            bank0.store_account(&address, &AccountSharedData::new(10, 0, &owner));
            bank0.freeze();
            if flush_before_capture {
                bank0.squash();
                bank0.force_flush_accounts_cache();
            }
            let snapshot = NotificationBank::new(bank0.clone());
            let bank1 = Bank::new_from_parent(bank0.clone(), SlotLeader::default(), 1);
            bank1.store_account(&address, &AccountSharedData::new(20, 0, &owner));
            bank1.squash();
            bank1.force_flush_accounts_cache();
            assert_eq!(snapshot.program_accounts(&owner)[0].1.lamports(), 10);
            assert_eq!(bank1.get_account(&address).unwrap().lamports(), 20);
            if !flush_before_capture {
                assert!(
                    bank0
                        .get_program_accounts_modified_since_parent(&owner)
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn test_retention_budget_expires_pending_references() {
        let genesis = create_genesis_config(1_000_000).genesis_config;
        let bank0 = Arc::new(Bank::new_for_tests(&genesis));
        bank0.freeze();
        let size = NotificationBank::new(bank0.clone()).bytes;
        let mut banks = NotificationBanks::new(size);
        let first = banks.retain(bank0.clone());
        assert!(first.bank.upgrade().is_some());
        let bank1 = Arc::new(Bank::new_from_parent(bank0, SlotLeader::default(), 1));
        bank1.freeze();
        banks.retain(bank1);
        assert!(banks.bytes <= size);
        assert!(first.bank.upgrade().is_none());
    }

    #[test]
    fn test_root_history_survives_multiple_squashes() {
        let genesis = create_genesis_config(1_000_000).genesis_config;
        let forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis));
        let mut retained = NotificationBanks::new(16 * 1024 * 1024);
        for slot in 1..=3 {
            let parent = forks.read().unwrap().root_bank();
            let bank = Bank::new_from_parent(parent, SlotLeader::default(), slot);
            bank.freeze();
            let bank = forks
                .write()
                .unwrap()
                .insert(bank)
                .clone_without_scheduler();
            retained.retain_roots(&[bank]);
            forks.write().unwrap().set_root(slot, None, None);
        }
        let chain = retained.chain(3, &forks.read().unwrap());
        assert_eq!(
            chain.banks.iter().map(|bank| bank.slot).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(chain.banks.iter().all(|bank| bank.bank.upgrade().is_some()));
        // Commitment can also arrive for an earlier root after it was pruned.
        let chain = retained.chain(2, &forks.read().unwrap());
        assert_eq!(
            chain.banks.iter().map(|bank| bank.slot).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}
