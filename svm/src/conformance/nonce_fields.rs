use solana_hash::Hash;

pub struct NonceFields {
    pub blockhash: Hash,
    pub blockhash_lamports_per_signature: u64,
}
