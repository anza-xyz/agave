use {solana_entry::entry::Entry, solana_runtime::bank::Bank, std::sync::Arc};

pub type WorkingBankEntry = (Arc<Bank>, (Entry, u64));
