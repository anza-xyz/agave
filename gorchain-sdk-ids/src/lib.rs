//! GorChain SDK IDs - Program addresses for GorChain network
//! 
//! This replaces solana-sdk-ids but keeps most program IDs the same for compatibility:
//! - System Program: Same as Solana (11111111111111111111111111111111)
//! - Native Token: GOR as the native token instead of SOL

use solana_pubkey::{declare_id, Pubkey};

/// System program ID - keeping same as Solana for compatibility
pub mod system_program {
    use super::*;
    declare_id!("11111111111111111111111111111111");
}

/// Vote program ID - keeping same as Solana for compatibility
pub mod vote {
    use super::*;
    declare_id!("Vote111111111111111111111111111111111111111");
}

/// Stake program ID - keeping same as Solana for compatibility  
pub mod stake {
    use super::*;
    declare_id!("Stake11111111111111111111111111111111111111");
}

/// Config program ID - keeping same as Solana for compatibility
pub mod config {
    use super::*;
    declare_id!("Config1111111111111111111111111111111111111");
}

/// Incinerator program ID - keeping same as Solana for compatibility
pub mod incinerator {
    use super::*;
    declare_id!("1nc1nerator11111111111111111111111111111111");
}

/// Address lookup table program ID - keeping same as Solana for compatibility
pub mod address_lookup_table {
    use super::*;
    declare_id!("AddressLookupTab1e1111111111111111111111111");
}

/// BPF loader program ID - keeping same as Solana for compatibility
pub mod bpf_loader {
    use super::*;
    declare_id!("BPFLoader1111111111111111111111111111111111");
}

/// BPF loader deprecated program ID - keeping same as Solana for compatibility
pub mod bpf_loader_deprecated {
    use super::*;
    declare_id!("BPFLoader1111111111111111111111111111111111");
}

/// BPF loader upgradeable program ID - keeping same as Solana for compatibility
pub mod bpf_loader_upgradeable {
    use super::*;
    declare_id!("BPFLoaderUpgradeab1e11111111111111111111111");
}

/// Loader v4 program ID - keeping same as Solana for compatibility
pub mod loader_v4 {
    use super::*;
    declare_id!("LoaderV411111111111111111111111111111111111");
}

/// Compute budget program ID - keeping same as Solana for compatibility
pub mod compute_budget {
    use super::*;
    declare_id!("ComputeBudget111111111111111111111111111111");
}

/// Ed25519 program ID - keeping same as Solana for compatibility
pub mod ed25519_program {
    use super::*;
    declare_id!("Ed25519SigVerify111111111111111111111111111");
}

/// Secp256k1 program ID - keeping same as Solana for compatibility
pub mod secp256k1_program {
    use super::*;
    declare_id!("KeccakSecp256k11111111111111111111111111111");
}

/// Feature gate program ID - keeping same as Solana for compatibility
pub mod feature_gate {
    use super::*;
    declare_id!("Feature111111111111111111111111111111111111");
}

/// Native loader program ID - keeping same as Solana for compatibility
pub mod native_loader {
    use super::*;
    declare_id!("NativeLoader1111111111111111111111111111111");
}

// Re-export commonly used IDs for compatibility
pub use {
    address_lookup_table::id as address_lookup_table_id,
    bpf_loader::id as bpf_loader_id,
    bpf_loader_deprecated::id as bpf_loader_deprecated_id,
    bpf_loader_upgradeable::id as bpf_loader_upgradeable_id,
    compute_budget::id as compute_budget_id,
    config::id as config_id,
    ed25519_program::id as ed25519_program_id,
    feature_gate::id as feature_gate_id,
    incinerator::id as incinerator_id,
    loader_v4::id as loader_v4_id,
    native_loader::id as native_loader_id,
    secp256k1_program::id as secp256k1_program_id,
    stake::id as stake_id,
    system_program::id as system_program_id,
    vote::id as vote_id,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_program_id() {
        // Verify system program ID is unchanged from Solana
        assert_eq!(
            system_program::id().to_string(),
            "11111111111111111111111111111111"
        );
    }

    #[test]
    fn test_other_program_ids() {
        // Verify other program IDs are unchanged
        assert_eq!(
            vote::id().to_string(),
            "Vote111111111111111111111111111111111111111"
        );
        assert_eq!(
            stake::id().to_string(),
            "Stake11111111111111111111111111111111111111"
        );
    }
}
