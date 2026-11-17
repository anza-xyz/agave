use {
    solana_pubkey::Pubkey,
    solana_sdk_ids::{
        bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4, native_loader,
    },
};

/// The owner of a programs accounts, thus the loader of a program
#[derive(Default, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Debug)]
pub enum ProgramCacheEntryOwner {
    #[default]
    NativeLoader,
    LoaderV1,
    LoaderV2,
    LoaderV3,
    LoaderV4,
}

impl TryFrom<&Pubkey> for ProgramCacheEntryOwner {
    type Error = ();
    fn try_from(loader_key: &Pubkey) -> Result<Self, ()> {
        if native_loader::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::NativeLoader)
        } else if bpf_loader_deprecated::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV1)
        } else if bpf_loader::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV2)
        } else if bpf_loader_upgradeable::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV3)
        } else if loader_v4::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV4)
        } else {
            Err(())
        }
    }
}

impl From<ProgramCacheEntryOwner> for Pubkey {
    fn from(program_cache_entry_owner: ProgramCacheEntryOwner) -> Self {
        match program_cache_entry_owner {
            ProgramCacheEntryOwner::NativeLoader => native_loader::id(),
            ProgramCacheEntryOwner::LoaderV1 => bpf_loader_deprecated::id(),
            ProgramCacheEntryOwner::LoaderV2 => bpf_loader::id(),
            ProgramCacheEntryOwner::LoaderV3 => bpf_loader_upgradeable::id(),
            ProgramCacheEntryOwner::LoaderV4 => loader_v4::id(),
        }
    }
}
