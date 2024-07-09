use {
    super::{AccountsDb, BinnedHashData, LoadedAccount},
    crate::{
        accounts_hash::{AccountHash, CalculateHashIntermediate},
        pubkey_bins::PubkeyBinCalculator24,
    },
    solana_sdk::{clock::Slot, hash::Hash, pubkey::Pubkey},
    std::{
        ops::Range,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
};

/// called on a struct while scanning append vecs
pub trait AppendVecScan: Send + Sync + Clone {
    /// return true if this pubkey should be included
    fn filter(&mut self, pubkey: &Pubkey) -> bool;
    /// set current slot of the scan
    fn set_slot(&mut self, slot: Slot);
    /// found `account` in the append vec
    fn found_account(&mut self, account: &LoadedAccount);
    /// scanning is done
    fn scanning_complete(self) -> BinnedHashData;
    /// initialize accumulator
    fn init_accum(&mut self, count: usize);
}

#[derive(Clone)]
/// state to keep while scanning append vec accounts for hash calculation
/// These would have been captured in a fn from within the scan function.
/// Some of these are constant across all pubkeys, some are constant across a slot.
/// Some could be unique per pubkey.
pub struct ScanState<'a> {
    /// slot we're currently scanning
    pub current_slot: Slot,
    /// accumulated results
    pub accum: BinnedHashData,
    pub bin_calculator: &'a PubkeyBinCalculator24,
    pub bin_range: &'a Range<usize>,
    pub range: usize,
    pub sort_time: Arc<AtomicU64>,
    pub pubkey_to_bin_index: usize,
}

impl<'a> AppendVecScan for ScanState<'a> {
    fn set_slot(&mut self, slot: Slot) {
        self.current_slot = slot;
    }
    fn filter(&mut self, pubkey: &Pubkey) -> bool {
        self.pubkey_to_bin_index = self.bin_calculator.bin_from_pubkey(pubkey);
        self.bin_range.contains(&self.pubkey_to_bin_index)
    }
    fn init_accum(&mut self, count: usize) {
        if self.accum.is_empty() {
            self.accum.append(&mut vec![Vec::new(); count]);
        }
    }
    fn found_account(&mut self, loaded_account: &LoadedAccount) {
        let pubkey = loaded_account.pubkey();
        assert!(self.bin_range.contains(&self.pubkey_to_bin_index)); // get rid of this once we have confidence

        // when we are scanning with bin ranges, we don't need to use exact bin numbers.
        // Subtract to make first bin we care about at index 0.
        self.pubkey_to_bin_index -= self.bin_range.start;

        let balance = loaded_account.lamports();
        let mut account_hash = loaded_account.loaded_hash();

        let hash_is_missing = account_hash == AccountHash(Hash::default());
        if hash_is_missing {
            let computed_hash = AccountsDb::hash_account_data(
                loaded_account.lamports(),
                loaded_account.owner(),
                loaded_account.executable(),
                loaded_account.rent_epoch(),
                loaded_account.data(),
                loaded_account.pubkey(),
            );
            account_hash = computed_hash;
        }
        let source_item = CalculateHashIntermediate {
            hash: account_hash,
            lamports: balance,
            pubkey: *pubkey,
        };
        self.init_accum(self.range);
        self.accum[self.pubkey_to_bin_index].push(source_item);
    }
    fn scanning_complete(mut self) -> BinnedHashData {
        let timing = AccountsDb::sort_slot_storage_scan(&mut self.accum);
        self.sort_time.fetch_add(timing, Ordering::Relaxed);
        self.accum
    }
}
