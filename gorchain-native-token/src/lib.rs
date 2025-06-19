//! GorChain Native Token Implementation
//! 
//! This module defines the GOR native token mint address and related constants.
//! GOR replaces SOL as the native token in the GorChain network.

use solana_pubkey::declare_id;

/// GOR token amount type for display purposes
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Sol(pub f64);

impl Sol {
    pub fn new(value: f64) -> Self {
        Sol(value)
    }
    
    /// Create Sol from lamports (u64)
    pub fn from_lamports(lamports: u64) -> Self {
        Sol(lamports_to_gor(lamports))
    }
}

impl std::fmt::Display for Sol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} GOR", self.0)
    }
}

impl From<f64> for Sol {
    fn from(value: f64) -> Self {
        Sol(value)
    }
}

impl From<u64> for Sol {
    fn from(lamports: u64) -> Self {
        Sol::from_lamports(lamports)
    }
}

impl From<Sol> for f64 {
    fn from(sol: Sol) -> Self {
        sol.0
    }
}

/// GOR native token mint address
/// This replaces the SOL native mint So11111111111111111111111111111111111111112
pub mod native_mint {
    use super::*;
    
    /// GOR native mint address: kLGKcrKaSHyiHTs2XnBZz4ejoVw6Cg77F4TQ2D8HuFa
    declare_id!("kLGKcrKaSHyiHTs2XnBZz4ejoVw6Cg77F4TQ2D8HuFa");
    
    /// Number of decimals for GOR token (same as SOL)
    pub const DECIMALS: u8 = 9;
    
    /// GOR native mint account data (same structure as SOL native mint)
    pub const ACCOUNT_DATA: [u8; 90] = [
        // Mint account data structure for native token
        1, 0, 0, 0, // mint_authority (Option<Pubkey>) - Some
        // GOR native mint authority (32 bytes) - self-referential
        0x47, 0x4f, 0x52, 0x31, 0x31, 0x31, 0x31, 0x31, // GOR11111...
        0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31,
        0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31,
        0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x32,
        255, 255, 255, 255, 255, 255, 255, 255, // supply (u64) - max supply
        255, 255, 255, 255, 255, 255, 255, 255,
        9, // decimals (u8)
        1, // is_initialized (bool)
        0, 0, 0, 0, // freeze_authority (Option<Pubkey>) - None
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
}

/// Maximum supply of GOR tokens (1 billion with 9 decimals)
pub const MAX_SUPPLY: u64 = 1_000_000_000_000_000_000; // 1 billion GOR

/// Conversion constants
pub const LAMPORTS_PER_GOR: u64 = 1_000_000_000; // 1 GOR = 1 billion lamports
pub const GOR_TO_LAMPORTS: u64 = LAMPORTS_PER_GOR;
pub const LAMPORTS_TO_GOR: f64 = 1.0 / (LAMPORTS_PER_GOR as f64);

// Compatibility aliases for solana-native-token API
pub const LAMPORTS_PER_SOL: u64 = LAMPORTS_PER_GOR;

/// Convert GOR to lamports
pub fn gor_to_lamports(gor: f64) -> u64 {
    (gor * LAMPORTS_PER_GOR as f64) as u64
}

/// Convert lamports to GOR
pub fn lamports_to_gor(lamports: u64) -> f64 {
    lamports as f64 * LAMPORTS_TO_GOR
}

// Compatibility aliases for solana-native-token API
pub fn sol_to_lamports(sol: f64) -> u64 {
    gor_to_lamports(sol)
}

pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports_to_gor(lamports)
}

/// Format lamports as GOR string
pub fn lamports_to_gor_string(lamports: u64) -> String {
    format!("{} GOR", lamports_to_gor(lamports))
}

/// Format lamports as GOR string with precision
pub fn lamports_to_gor_string_with_precision(lamports: u64, precision: usize) -> String {
    format!("{:.precision$} GOR", lamports_to_gor(lamports), precision = precision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_native_mint_id() {
        assert_eq!(
            native_mint::id().to_string(),
            "kLGKcrKaSHyiHTs2XnBZz4ejoVw6Cg77F4TQ2D8HuFa"
        );
    }

    #[test]
    fn test_gor_conversions() {
        assert_eq!(gor_to_lamports(1.0), LAMPORTS_PER_GOR);
        assert_eq!(lamports_to_gor(LAMPORTS_PER_GOR), 1.0);
        assert_eq!(gor_to_lamports(0.5), LAMPORTS_PER_GOR / 2);
        assert_eq!(lamports_to_gor(LAMPORTS_PER_GOR / 2), 0.5);
    }

    #[test]
    fn test_gor_formatting() {
        assert_eq!(lamports_to_gor_string(LAMPORTS_PER_GOR), "1 GOR");
        assert_eq!(lamports_to_gor_string(LAMPORTS_PER_GOR / 2), "0.5 GOR");
        assert_eq!(
            lamports_to_gor_string_with_precision(LAMPORTS_PER_GOR / 3, 6),
            "0.333333 GOR"
        );
    }

    #[test]
    fn test_max_supply() {
        assert_eq!(MAX_SUPPLY, 1_000_000_000_000_000_000);
        assert_eq!(lamports_to_gor(MAX_SUPPLY), 1_000_000_000.0); // 1 billion GOR
    }
}
